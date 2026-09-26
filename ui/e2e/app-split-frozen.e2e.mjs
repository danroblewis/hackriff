// T-1004 (MMAP split view): with **pane 1 frozen on the past and pane 2 live**, the controls that
// act on one pane stop lying about which pane, and about what they can deliver. The user
// (2026-09-25): "A common use would be to look at one signal from the past and the current
// waterfall, and each split needs its own 'Live' button." T-1000 made the active pane visible; this
// spec is the four behaviours that were still wrong once it was, in the real app over the mock SDR:
//
//  1. **Go-to on a frozen pane offers "go live at this frequency".** It used to land the pane in the
//     past and paint an offer disabled as "frozen behind the growing edge" — a dead end for the user
//     who had just typed a frequency into it. The same offer is now takeable as a go-live: the pane
//     returns to the live edge and the front end is tuned there, in one press, and the BUTTON says
//     so ("Go live here", not "Retune").
//  2. **Record IQ says what it will record on a frozen pane.** An IQ recording runs forward from
//     now; on a frozen viewport the button said "Record IQ" and recorded the live band. It now reads
//     "Record IQ (live)" and states the band and the reason — and reads plainly again on a live pane.
//  3. **The output badges are per pane.** A box carrying an open output shows its badge in EVERY
//     pane that draws it, each placed through that pane's own mapping — not once, on whichever pane
//     happened to place it first.
//  4. **A selection is the selection in one pane and a linked GHOST in the other.** Selection is one
//     piece of page state; drawn as "selected" everywhere it said the user had selected twice.
//
// And the set rule for every split-view ticket: **nothing in pane 2 changes when pane 1 is touched.**
//
// Unit tier: `ui/test/surface-retune.test.ts` (§5, the go-live offer and its guards),
// `ui/test/app-capture-window.test.ts` (the Record IQ wording), `ui/test/surface-features.test.ts`
// (§4b, the linked ghost), `ui/test/app-map-controls.test.ts` (the offer button's word).
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";
import { paneAct } from "./app-chrome.mjs";

const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 36;
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const ART = SHOTS ?? path.join(process.cwd(), "e2e", "artifacts");
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
/** Outside the fixture's own 2.4 MHz window, so no tuned window covers it and an offer is painted. */
const AWAY = "433.92M";
/** The second width's, far from BOTH the fixture's window and the one the first test's go-live
 * leaves the shared mock tuned to: an offer is only painted where no tuned window covers the pane. */
const AWAY_2 = "915M";

/** Each pane row's follow state and internal id, keyed by the id the status row prints. */
const FOLLOWS = `JSON.stringify(Object.fromEntries([...document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]')]
  .map((r) => [r.querySelector('.hk-surface-id').textContent, r.dataset.following === 'true'])))`;
const follows = async (page) => JSON.parse(await page.eval(FOLLOWS));
/** The Record IQ item in the viewport menu, as the user reads it. */
const RECORD = `JSON.stringify((() => { const b = document.querySelector('#map-pane-menu [data-pane-act="record"]');
  return b ? { text: b.textContent, title: b.title, scope: b.dataset.scope } : null; })())`;
const OFFER = `JSON.stringify((() => { const o = document.querySelector('.map-offer');
  const g = document.querySelector('.map-offer-go');
  return { shown: !o.hidden, why: document.querySelector('.map-offer-why').textContent, go: g.textContent, disabled: !!g.disabled }; })())`;
/** Every placed feature button, with the pane it is on and the state it is drawn in. */
const PINS = `JSON.stringify([...document.querySelectorAll('.sf-pins .sf-pin')].map((p) => ({
  id: p.dataset.pin, pane: p.dataset.pane, sel: p.classList.contains('selected'), linked: p.classList.contains('linked'),
  label: p.getAttribute('aria-label'), pressed: p.getAttribute('aria-pressed') })))`;
const BADGES = `JSON.stringify([...document.querySelectorAll('.sf-obadges .sf-obadge')].map((b) => ({ id: b.dataset.pin, pane: b.dataset.pane, kinds: b.dataset.kinds })))`;
const ACTIVE = "document.querySelector('.sf-active-pane').dataset.pane";
const ACTIVE_ID = "document.querySelector('.sf-active-pane').dataset.paneId";
const MENU_LABELS = `JSON.stringify([...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item .lbl')].map((l) => l.textContent))`;
const menuItem = (label) => `[...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item')].find((b) => b.querySelector('.lbl')?.textContent === ${JSON.stringify(label)})`;

/** Right-click the box `id`, re-located at the instant of each try, and wait for its menu. */
async function openMenuOn(page, id) {
  for (let tries = 0; tries < 8; tries++) {
    const at = JSON.parse(await page.eval(`JSON.stringify((() => {
      const el = document.querySelector('.sf-pins .sf-pin.area[data-pin=${JSON.stringify(id)}]'); if (!el) return null;
      const r = el.getBoundingClientRect();
      const x = r.x + Math.min(r.width / 2, 40), y = r.y + Math.min(r.height / 2, 20);
      return document.elementFromPoint(x, y)?.classList.contains('sf-canvas') ? { x, y } : null; })())`));
    if (at) {
      await page.mouse("mouseMoved", at.x, at.y);
      await page.mouse("mousePressed", at.x, at.y, { button: "right", buttons: 2, clickCount: 1 });
      await page.mouse("mouseReleased", at.x, at.y, { button: "right", buttons: 0, clickCount: 1 });
      if (await page.eval("!!document.querySelector('.ctx-menu:not([hidden])')")) return JSON.parse(await page.eval(MENU_LABELS));
    }
    await new Promise((r) => setTimeout(r, 200));
  }
  throw new Error(`right-clicking box ${id} never opened its menu`);
}

/** Open the viewport menu, read the Record IQ item, close it again. */
async function recordItem(page) {
  await page.click("document.querySelector('.map-pane-btn')");
  await page.waitFor("the viewport menu to open", "!document.querySelector('#map-pane-menu').hidden", { timeoutMs: 5000 });
  const r = JSON.parse(await page.eval(RECORD));
  await page.click("document.querySelector('.map-pane-btn')");
  await page.waitFor("the viewport menu to close", "document.querySelector('#map-pane-menu').hidden", { timeoutMs: 5000 });
  return r;
}

/** Make pane `n` active with its key, and confirm the outline moved (T-1000's own guarantee). */
async function activate(page, n) {
  await page.key(String(n), { code: `Digit${n}`, keyCode: 48 + n });
  await page.waitFor(`pane ${n} to be active`, `${ACTIVE} === ${JSON.stringify(String(n))}`, { timeoutMs: 10000 });
  return page.eval(ACTIVE_ID);
}

/** A feature's hit area big enough to press, inside the pane rectangle `[x0, x1]`, clear of the
 * floating chrome (the canvas must be what a press at the point lands on). Confirmed first, then
 * newest — a candidate can be merged or expire mid-spec, and then its box is gone. */
const BOX_IN = (x0, x1) => `JSON.stringify((() => {
  const c = document.querySelector('.sf-canvas').getBoundingClientRect();
  let best = null;
  for (const p of document.querySelectorAll('.sf-pins .sf-pin.detection.area')) {
    const r = p.getBoundingClientRect();
    const ax0 = Math.max(r.x, ${x0} + 8, c.x + 24), ax1 = Math.min(r.right, ${x1} - 8, c.right - 24);
    const ay0 = Math.max(r.y, c.y + 8), ay1 = Math.min(r.bottom, c.bottom - 8);
    if (ax1 - ax0 < 14 || ay1 - ay0 < 14) continue;
    const x = (ax0 + ax1) / 2, y = ay0 + Math.min(24, (ay1 - ay0) / 2);
    const top = document.elementFromPoint(x, y);
    if (!top || !top.classList.contains('sf-canvas')) continue;
    const conf = p.classList.contains('confirmed');
    if (!best || (conf && !best.conf) || (conf === best.conf && r.y < best.y)) best = { id: p.dataset.pin, pane: p.dataset.pane, x, y, conf };
  }
  return best; })())`;

// One mock-SDR backend for both widths (lane base + 36; +8/+12/+16/+20/+24/+28/+32 are taken).
let backendP = null;
const mockBackend = () => (backendP ??= startBackend({ port: PORT, mockDevice: true }));
after(async () => { (await backendP?.catch(() => null))?.stop(); });

test("split view, pane 1 frozen and pane 2 live: Go-to offers go-live, Record IQ says what it records, badges and the linked ghost are per pane", async (t) => {
  const backend = await mockBackend();
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: 1280, height: 800 });
  const shot = async (name) => { await page.frames(2); await page.shot(path.join(ART, `split-frozen-${name}.png`)); };
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the floating cluster to mount", "!!document.querySelector('.map-pane-btn') && !!document.querySelector('.sf-active-pane')", { timeoutMs: 30000 });
  await page.waitFor("a pane row with its level stated",
    "!!document.querySelector('.hk-surface-viewport[data-viewport=\"pane\"] .hk-surface-level')?.textContent", { timeoutMs: 90000 });

  // ---- the arrangement: two panes, pane 1 frozen on the past, pane 2 following the live edge ----
  await paneAct(page, "split");
  await page.waitFor("two panes", `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length === 2`, { timeoutMs: 10000 });
  const pane1 = await activate(page, 1);
  // `L` is the FAB's press on the active pane: it pins a pane that is not at the live edge and
  // freezes one that is, so it is pressed until pane 1 reports frozen (never a sleep).
  for (let i = 0; i < 4 && (await follows(page))[pane1] !== false; i++) {
    await page.key("l", { code: "KeyL", keyCode: 76 });
    await page.frames(3);
  }
  const arranged = await follows(page);
  const pane2 = Object.keys(arranged).find((id) => id !== pane1);
  t.diagnostic(`arranged: ${JSON.stringify({ arranged, pane1, pane2 })}`);
  assert.equal(arranged[pane1], false, "pane 1 did not freeze");
  assert.equal(arranged[pane2], true, "pane 2 is not following the live edge");
  await shot("1-arranged");

  // ---- (2) Record IQ: the frozen pane's press records the LIVE band, and says so before it is pressed ----
  const frozenRec = await recordItem(page);
  t.diagnostic(`Record IQ, pane 1 frozen: ${JSON.stringify(frozenRec)}`);
  assert.equal(frozenRec.scope, "frozen");
  assert.equal(frozenRec.text, "Record IQ (live)", "a frozen viewport's Record IQ still reads as 'Record IQ'");
  assert.match(frozenRec.title, /pane 1 of 2 is frozen behind the live edge/);
  assert.match(frozenRec.title, /not the past window on screen/);
  assert.match(frozenRec.title, /Record raw IQ forward from now over [\d.]+–[\d.]+ MHz/, "it does not say WHICH band it will record");
  // The live pane's is the plain one — the wording follows the pane, not the split.
  await activate(page, 2);
  const liveRec = await recordItem(page);
  t.diagnostic(`Record IQ, pane 2 live: ${JSON.stringify(liveRec)}`);
  assert.equal(liveRec.scope, "live");
  assert.equal(liveRec.text, "Record IQ");
  assert.doesNotMatch(liveRec.title, /frozen/);

  // ---- (4) the selection: selected in the pane it was made in, a linked ghost in the other ----
  const canvas = await page.$rect(".sf-canvas");
  const mid = canvas.x + canvas.w / 2;
  await page.waitFor("a pressable detection box on the live pane", `${BOX_IN(mid, canvas.x + canvas.w)} !== 'null'`, { timeoutMs: 120000, everyMs: 500 });
  const target = JSON.parse(await page.eval(BOX_IN(mid, canvas.x + canvas.w)));
  assert.ok(target, "no detection box on pane 2 to select");
  await page.mouse("mousePressed", target.x, target.y, { buttons: 1, clickCount: 1 });
  await page.mouse("mouseReleased", target.x, target.y, { buttons: 0, clickCount: 1 });
  await page.waitFor("the box to be selected in the pane it was pressed in",
    `JSON.parse(${PINS}).some((p) => p.id === ${JSON.stringify(target.id)} && p.sel)`, { timeoutMs: 20000 });
  const pins = JSON.parse(await page.eval(PINS)).filter((p) => p.id === target.id);
  t.diagnostic(`selection across panes: ${JSON.stringify(pins)}`);
  const owner = pins.find((p) => p.sel), ghost = pins.find((p) => p.linked);
  assert.ok(owner, "the pressed box is not selected anywhere");
  assert.equal(pins.filter((p) => p.sel).length, 1, "the same feature is drawn as THE selection in more than one pane");
  if (pins.length > 1) {
    // The feature is placed in both panes (the usual case: the split copies the window), so the
    // other pane must show it as the link, not as a second selection.
    assert.ok(ghost, `the other pane drew the selection as ${JSON.stringify(pins)}`);
    assert.notEqual(ghost.pane, owner.pane);
    assert.equal(ghost.sel, false);
    assert.equal(ghost.pressed, "false", "the ghost reads as pressed to a screen reader");
    assert.match(ghost.label, /linked: selected in another viewport/);
  } else {
    t.diagnostic("the feature is placed in one pane only this frame — the ghost has nothing to draw");
  }
  await shot("2-selection-ghost");

  // ---- (3) the output badges, per pane: one recording, a badge in every pane that draws the box ----
  // The box is re-located before each try: the selection raised the sheet over part of the canvas,
  // and a live pane moves its boxes between the read and the press.
  const labels = await openMenuOn(page, target.id);
  assert.ok(labels.includes("Record clip"), `the box menu lacks Record clip: ${labels}`);
  await page.click(menuItem("Record clip"));
  await page.waitFor("the box to carry its recording badge",
    `JSON.parse(${BADGES}).some((b) => b.id === ${JSON.stringify(target.id)} && /\\brec\\b/.test(b.kinds))`,
    { timeoutMs: 60000, everyMs: 250 });
  // Placed THIS frame: a badge can only be drawn on a pane that is drawing the box.
  const state = JSON.parse(await page.eval(`JSON.stringify({ pins: JSON.parse(${PINS}), badges: JSON.parse(${BADGES}) })`));
  const drawn = state.pins.filter((p) => p.id === target.id).map((p) => p.pane).sort();
  const badged = state.badges.filter((b) => b.id === target.id).map((b) => b.pane).sort();
  t.diagnostic(`badges per pane: drawn on ${JSON.stringify(drawn)}, badged on ${JSON.stringify(badged)}`);
  assert.deepEqual(badged, drawn, "the badge is not drawn in every pane that draws the box");
  await shot("3-badges-per-pane");

  // ---- (1) Go-to on the FROZEN pane offers "go live at this frequency" ----
  const before = await follows(page);
  await activate(page, 1);
  await page.eval(`(() => { const i = document.querySelector('.map-goto input'); i.value = ${JSON.stringify(AWAY)};
    i.closest('form').requestSubmit(); })()`);
  await page.waitFor("the offer to be painted", "!document.querySelector('.map-offer').hidden", { timeoutMs: 10000 });
  const offer = JSON.parse(await page.eval(OFFER));
  t.diagnostic(`the frozen pane's offer: ${JSON.stringify(offer)}`);
  assert.equal(offer.go, "Go live here", "the button offers a plain Retune on a frozen pane");
  assert.equal(offer.disabled, false, "the frozen pane's offer is still a dead end");
  assert.match(offer.why, /This viewport is frozen/);
  assert.match(offer.why, /Go live at 433\.9\d* MHz/);
  assert.match(offer.why, /returns to the live edge/);
  await shot("4-go-live-offer");

  // Taking it does both halves: pane 1 comes back to the live edge AND the radio is asked for the
  // frequency that was typed — one command, through the one gate.
  const controlsBefore = page.requests.filter((r) => CONTROL.test(r.url)).length;
  await page.click("document.querySelector('.map-offer-go')");
  await page.waitFor("pane 1 to return to the live edge",
    `(${FOLLOWS.replace(/^JSON\.stringify/, "")})[${JSON.stringify(pane1)}] === true`, { timeoutMs: 20000 });
  // The command is the page's own request, so it is waited for on this side rather than in the page.
  for (let i = 0; i < 100 && page.requests.filter((r) => CONTROL.test(r.url)).length <= controlsBefore; i++) {
    await new Promise((r) => setTimeout(r, 200));
  }
  const commands = page.requests.filter((r) => CONTROL.test(r.url));
  t.diagnostic(`device commands: ${JSON.stringify(commands.map((r) => r.url))}`);
  assert.ok(commands.length > controlsBefore, "the go-live never reached the one device gate");
  // The set rule: pane 2 was not touched by any of it.
  const after = await follows(page);
  assert.equal(after[pane2], before[pane2], "pane 2's follow state changed while pane 1 was worked on");
  await shot("5-went-live");

  assert.deepEqual(page.exceptions, [], "uncaught exception");
});

test("at 400 px the same split says the same things: pane 1 frozen reads 'Record IQ (live)' and Go-to offers go-live", async (t) => {
  const backend = await mockBackend();
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: 400, height: 820 });
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the floating cluster to mount", "!!document.querySelector('.map-pane-btn') && !!document.querySelector('.sf-active-pane')", { timeoutMs: 30000 });
  await page.waitFor("a pane row with its level stated",
    "!!document.querySelector('.hk-surface-viewport[data-viewport=\"pane\"] .hk-surface-level')?.textContent", { timeoutMs: 90000 });
  await paneAct(page, "split");
  await page.waitFor("two panes", `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length === 2`, { timeoutMs: 10000 });
  const pane1 = await activate(page, 1);
  for (let i = 0; i < 4 && (await follows(page))[pane1] !== false; i++) {
    await page.key("l", { code: "KeyL", keyCode: 76 });
    await page.frames(3);
  }
  assert.equal((await follows(page))[pane1], false, "pane 1 did not freeze at 400 px");

  const rec = await recordItem(page);
  t.diagnostic(`400 px, Record IQ on the frozen pane: ${JSON.stringify(rec)}`);
  assert.equal(rec.scope, "frozen");
  assert.equal(rec.text, "Record IQ (live)");
  assert.match(rec.title, /frozen behind the live edge/);

  await page.eval(`(() => { const i = document.querySelector('.map-goto input'); i.value = ${JSON.stringify(AWAY_2)};
    i.closest('form').requestSubmit(); })()`);
  await page.waitFor("the offer to be painted", "!document.querySelector('.map-offer').hidden", { timeoutMs: 10000 });
  const offer = JSON.parse(await page.eval(OFFER));
  t.diagnostic(`400 px offer: ${JSON.stringify(offer)}`);
  assert.match(offer.why, /Go live at 915\.0\d* MHz/);
  assert.equal(offer.go, "Go live here");
  assert.equal(offer.disabled, false);
  await page.frames(2);
  await page.shot(path.join(ART, "split-frozen-400.png"));
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
