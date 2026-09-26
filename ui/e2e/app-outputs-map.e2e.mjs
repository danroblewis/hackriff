// T-994: outputs on the map, in the real app over the MOCK SDR. The user (2026-09-25): "if a box is
// actively being demodulated / listened / decoded, the box could show that it's active somehow, and
// there could be a right-click menu for it." The Outputs dock bar is retired. What the user sees:
//
//  1. No fixed-height bar at the bottom — at 1280 x 800 and at 400 px — while nothing is open.
//  2. Right-click a detection's BOX on the canvas → its menu, carrying Listen / Decode / Record clip /
//     Stream out / Analyze / Promote-or-Delete / Go to (the explorer found Listen unreachable from
//     Candidate rows; this is where it lives now).
//  3. Listen from that menu → once the SERVER's stream header arrives, the box shows the active halo
//     (pixels, drawn by the GL overlay pass) and the audio badge within a frame; the small
//     Active-outputs strip carries the former dock actions (Mute / Copy address / Stop).
//  4. Stop from the same menu → halo, badge and strip all drop.
//  5. Decode from the menu → while the backend reports the pipeline RUNNING the box carries the
//     decode badge; once the pipeline is done (stopped) the badge drops.
//  6. At 400 px a touch LONG-PRESS on a box opens the same menu, inside the viewport.
//
// Unit tier: `ui/test/app-outputs-map.test.ts` (activity model, halo geometry, badge layout).
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

// Its own mock-SDR backend (the lane's shared one is a plain --replay), at the lane's base + 28:
// +8/+12/+16/+20/+24 are fog-of-war, scan-everything, surface-retune, ring-drop and shadow-level.
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 28;
const SHOTS = process.env.HK_E2E_SHOTS ?? null;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
const ACTIVE_RGB = [245, 84, 158]; // marks.ts ACTIVE_MARK (0.96, 0.33, 0.62)

/** A press point on a box's hit rectangle that the CANVAS actually receives: the point under it is
 * `.sf-canvas` and not a piece of floating chrome. Scans DOWN the box's visible extent rather than
 * taking the box's top — at 400 px every box starts at the canvas top and the mode strip (`.mode`)
 * floats over exactly that band, so the single point 24 px below the top is the mode button on
 * every box but the leftmost (measured on main at 6f38fbb8 with a freshly built `hk`: pins at
 * x=157 and x=250 both answered `mode` at y=122 and `sf-canvas` from y=178 down, so step 6 could
 * never find a pressable box and timed out at 60 s — the T-1002/T-1004 red). Pressing where the
 * box IS exposed is the same act, not a weaker one: still a press on the canvas, over that box. */
const PRESS_IN = `(r, c, minW, minH) => {
  const x0 = Math.max(r.x, c.x + 24), x1 = Math.min(r.right, c.right - 24);
  const y0 = Math.max(r.y, c.y + 8), y1 = Math.min(r.bottom, c.bottom - 8);
  if (x1 - x0 < minW || y1 - y0 < minH) return null;
  const x = (x0 + x1) / 2;
  for (let y = y0 + Math.min(24, (y1 - y0) / 2); y <= y1; y += 24) {
    const top = document.elementFromPoint(x, y);
    if (top && top.classList.contains('sf-canvas')) return { x, y };
  }
  return null;
}`;

/** The detection box (a `.sf-pin.area` hit area over the box the overlay draws) best placed to be
 * pressed: wide and tall enough, clear of the floating chrome (the canvas is what is under its
 * press point — [[PRESS_IN]]), newest first — and, when `inside` names the tuned window, well
 * inside it, because Listen demodulates live IQ and the server rightly refuses (409) a channel
 * outside the window. Returns its id, a press point inside it and its area. */
let inside = null; // { lo, hi } Hz, set once the tuning is read
const BOX_AT = (minW = 16, minH = 16) => `(() => {
  const c = document.querySelector('.sf-canvas').getBoundingClientRect();
  const pressIn = ${PRESS_IN};
  const inside = ${JSON.stringify(inside)};
  let best = null;
  for (const p of document.querySelectorAll('.sf-pins .sf-pin.detection.area')) {
    const mhz = Number(/at ([\\d.]+) MHz/.exec(p.getAttribute('aria-label') ?? '')?.[1]);
    if (inside && !(mhz * 1e6 > inside.lo && mhz * 1e6 < inside.hi)) continue;
    const r = p.getBoundingClientRect();
    const at = pressIn(r, c, ${minW}, ${minH});
    if (!at) continue;
    // Confirmed first: a Candidate may be merged or expire mid-spec (candidates churn by design), and
    // then its box is gone with its badge. Then newest.
    const conf = p.classList.contains('confirmed');
    if (!best || (conf && !best.conf) || (conf === best.conf && r.y < best.area.y0)) best = { id: p.dataset.pin, x: at.x, y: at.y, conf, area: { x0: r.x, y0: r.y, x1: r.right, y1: r.bottom } };
  }
  return best;
})()`;
const box = async (page, minW, minH) => JSON.parse(await page.eval(`JSON.stringify(${BOX_AT(minW, minH)})`));

/** Wait for a pressable box and RETURN THAT BOX — chosen in the evaluation that answered, not in a
 * second one. `waitFor(!!BOX_AT())` followed by `box()` asks twice, and the boxes churn by design
 * (T-1049 measured a box's rectangle moving hundreds of pixels between two polls), so the second
 * ask could answer `null` after the first said yes: `Cannot read properties of null (reading 'id')`.
 * Same predicate, same timeout — only asked once. */
async function waitBox(page, why, { minW, minH, timeoutMs = 120000, everyMs = 500 } = {}) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const b = await box(page, minW, minH);
    if (b) return b;
    if (Date.now() >= deadline) throw new Error(`timed out after ${timeoutMs} ms waiting for ${why}`);
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/** Fixed-position bars pinned to the bottom edge spanning most of the width. The T-803 sheet is
 * excluded by name: its peek strip is the Selected panel's own closeable overlay, not an outputs
 * dock; so is the transient toast (a notice that fades, never a bar). */
const BOTTOM_BARS = `JSON.stringify([...document.querySelectorAll('body *')].filter((e) => {
  if (e.closest('.sheet') || e.id === 'toast') return false;
  const cs = getComputedStyle(e); if (cs.position !== 'fixed' || cs.display === 'none' || cs.visibility === 'hidden' || cs.opacity === '0') return false;
  const r = e.getBoundingClientRect();
  return r.height > 0 && r.height < 160 && r.bottom >= innerHeight - 16 && r.width >= innerWidth * 0.6;
}).map((e) => String(e.className || e.tagName)))`;

const MENU_LABELS = `JSON.stringify([...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item .lbl')].map((l) => l.textContent))`;
const menuItem = (label) => `[...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item')].find((b) => b.querySelector('.lbl')?.textContent === ${JSON.stringify(label)})`;
const menuItemMatching = (re) => `[...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item')].find((b) => ${re}.test(b.querySelector('.lbl')?.textContent ?? ''))`;
const BADGE = (id) => `document.querySelector('.sf-obadges .sf-obadge[data-pin=${JSON.stringify(id)}]')`;
const KINDS = (id) => `(${BADGE(id)}?.dataset.kinds ?? '')`;
/** Whether box `id` is placed on the canvas this frame. On the mock SDR the feature layer can go
 * without a box for tens of seconds while its row stays listed (observed on main at 2033e9dd, with
 * or without this change): a box can only carry a badge while it is drawn, so each check below
 * first waits for the box to BE drawn, then asserts on that same frame. */
const PLACED = (id) => `!!document.querySelector('.sf-pins .sf-pin.detection.area[data-pin=${JSON.stringify(id)}]')`;
const waitPlaced = (page, id, fail) => page.waitFor(`box ${id.slice(0, 8)} to be drawn`, PLACED(id), { timeoutMs: 120000, everyMs: 250 }).catch(fail);
const ACTIVE_BTN = (id) => `!!document.querySelector('.sf-pins .sf-pin.active[data-pin=${JSON.stringify(id)}]')`;

/** Pixels near ACTIVE_MARK in the band just OUTSIDE a box's left edge (where no badge sits — the
 * badge is at the top-right), i.e. the GL halo, not the DOM. Reports the whole frame's near-mark
 * pixels too, so a red says WHICH failure it is: `total: 0` = the halo was never drawn this frame;
 * a total with an empty band = it was drawn somewhere this band does not cover. (Both shapes were
 * observed on main at 6f38fbb8 while triaging T-1049; the message used to say only `0 → 0`.) */
async function haloScan(page, area) {
  const img = await page.shot();
  const x0 = Math.max(0, Math.floor(area.x0 - 12)), x1 = Math.max(0, Math.floor(area.x0) - 1);
  const y0 = Math.max(0, Math.floor(area.y0 + 16)), y1 = Math.min(img.height - 1, Math.floor(area.y1 - 4));
  const near = (i) => Math.abs(img.data[i] - ACTIVE_RGB[0]) + Math.abs(img.data[i + 1] - ACTIVE_RGB[1])
    + Math.abs(img.data[i + 2] - ACTIVE_RGB[2]) < 60;
  let n = 0;
  for (let y = y0; y <= y1; y++) for (let x = x0; x <= x1; x++) if (near((y * img.width + x) * 4)) n++;
  let total = 0, bx0 = Infinity, by0 = Infinity, bx1 = -1, by1 = -1;
  for (let y = 0; y < img.height; y++) for (let x = 0; x < img.width; x++) {
    if (!near((y * img.width + x) * 4)) continue;
    total++;
    if (x < bx0) bx0 = x; if (x > bx1) bx1 = x; if (y < by0) by0 = y; if (y > by1) by1 = y;
  }
  return { n, total, band: [x0, y0, x1, y1], bbox: total ? [bx0, by0, bx1, by1] : null };
}

async function rightClick(page, at) {
  await page.mouse("mouseMoved", at.x, at.y);
  await page.mouse("mousePressed", at.x, at.y, { button: "right", buttons: 2, clickCount: 1 });
  await page.mouse("mouseReleased", at.x, at.y, { button: "right", buttons: 0, clickCount: 1 });
}

/** Right-click the box `id` (re-located this instant: a following pane moves it), and wait for its menu. */
async function openMenuOn(page, id) {
  for (let tries = 0; tries < 8; tries++) {
    const b = await box(page, 8, 8);
    const at = b && b.id === id ? b : JSON.parse(await page.eval(`JSON.stringify((() => {
      const el = document.querySelector('.sf-pins .sf-pin.area[data-pin=${JSON.stringify(id)}]'); if (!el) return null;
      const c = document.querySelector('.sf-canvas').getBoundingClientRect();
      // Same rule as BOX_AT: a point the canvas receives, not the box's top under the chrome.
      return (${PRESS_IN})(el.getBoundingClientRect(), c, 8, 8); })())`));
    if (!at) throw new Error(`box ${id} is no longer on the canvas`);
    await rightClick(page, at);
    const open = await page.eval(`!!document.querySelector('.ctx-menu:not([hidden])')`);
    if (open) return JSON.parse(await page.eval(MENU_LABELS));
    await new Promise((r) => setTimeout(r, 150));
  }
  throw new Error(`right-clicking box ${id} never opened its menu`);
}

const authed = (token, method, url) => `fetch(${JSON.stringify(url)}, { method: ${JSON.stringify(method)}, headers: { Authorization: 'Bearer ${token}' } }).then((r) => r.json().catch(() => ({})))`;

test("outputs live on the map: right-click a box → Listen shows active + badge; Stop drops it; decode badges while it runs; no dock bar; long-press at 400 px", async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: 1280, height: 800 });
  const shot = async (name) => { if (SHOTS) await page.shot(path.join(SHOTS, `outputs-map-${name}.png`)); };
  const state = async () => page.eval(`JSON.stringify({
    menu: ${MENU_LABELS}, strip: document.querySelector('.out-strip')?.hidden, chips: [...document.querySelectorAll('.out-strip .out')].map((o) => [o.dataset.state, o.textContent]),
    pins: [...document.querySelectorAll('.sf-pins .sf-pin.detection')].slice(0, 12).map((p) => { const r = p.getBoundingClientRect(); return [p.className, p.getAttribute('aria-label'), Math.round(r.x), Math.round(r.y), Math.round(r.width), Math.round(r.height)]; }),
    chrome: document.querySelector('.sf-chrome')?.textContent?.slice(0, 200),
    badges: [...document.querySelectorAll('.sf-obadge')].map((b) => [b.dataset.pin?.slice(0, 8), b.dataset.kinds]),
    toast: document.querySelector('#toast')?.textContent, mode: document.querySelector('.mode[aria-pressed=true]')?.dataset.mode })`);
  const fail = async (e) => { throw new Error(`${e.message}\nstate: ${await state()}`); };

  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 120000 });
  const ctl = JSON.parse(await page.eval(`${authed(backend.token, "GET", "/api/control/state")}.then((r) => JSON.stringify(r.tuning ?? r.devices?.[0]?.tuning ?? null))`));
  if (ctl && ctl.center_hz && ctl.sample_rate_hz) inside = { lo: ctl.center_hz - 0.3 * ctl.sample_rate_hz, hi: ctl.center_hz + 0.3 * ctl.sample_rate_hz };
  t.diagnostic(`tuned: ${JSON.stringify(ctl)} → boxes chosen inside ${JSON.stringify(inside)}`);
  await waitBox(page, "blind detection to put a pressable box on the canvas").catch(fail);
  await page.frames(3);

  // (1) No dock bar, nothing reserved at the bottom while nothing is open.
  assert.equal(await page.eval(`document.querySelector('.dock, footer[data-slot="outputs"]')`), null, "the Outputs dock is retired");
  assert.equal(await page.eval(`document.querySelector('.out-strip').hidden`), true, "the Active-outputs strip exists only while something is open");
  assert.deepEqual(JSON.parse(await page.eval(BOTTOM_BARS)), [], "a fixed bar still spans the bottom at 1280 x 800");
  await shot("1-no-dock");

  // (2) Right-click a box → its menu, with every action named.
  const target = await waitBox(page, "a pressable box to right-click").catch(fail);
  const id = target.id;
  const haloBefore = await haloScan(page, target.area);
  const labels = await openMenuOn(page, id);
  t.diagnostic(`menu on ${id.slice(0, 8)}: ${labels.join(" | ")}`);
  for (const want of ["Listen", "Record clip", "Stream out", "Analyze", "Go to"]) assert.ok(labels.includes(want), `the box menu lacks ${want}: ${labels}`);
  assert.ok(labels.some((l) => /^Decode/.test(l)), `the box menu lacks Decode: ${labels}`);
  assert.ok(labels.some((l) => /^Delete/.test(l)), `the box menu lacks Delete: ${labels}`);
  await shot("2-menu");

  // (3) Listen → active + audio badge, within a frame of the server's header.
  await page.click(menuItem("Listen"));
  await page.waitFor("the server's stream header (the chip goes live)", `document.querySelector('.out-strip .out[data-state=live]') !== null`, { timeoutMs: 30000 }).catch(fail);
  await page.frames(2);
  await waitPlaced(page, id, fail);
  assert.match(await page.eval(KINDS(id)), /\baudio\b/, "the box carries the audio badge on every frame it is drawn once the header is in");
  assert.equal(await page.eval(ACTIVE_BTN(id)), true, "the box's feature is in the active state");
  assert.match(await page.eval(`document.querySelector('.sf-pin[data-pin=${JSON.stringify(id)}]').getAttribute('aria-label')`), /active: listening/);
  const area = JSON.parse(await page.eval(`JSON.stringify((() => { const r = document.querySelector('.sf-pins .sf-pin.area[data-pin=${JSON.stringify(id)}]')?.getBoundingClientRect();
    return r ? { x0: r.x, y0: r.y, x1: r.right, y1: r.bottom } : null; })())`)) ?? target.area;
  const haloOn = await haloScan(page, area);
  const halos = `before ${JSON.stringify(haloBefore)} (box ${JSON.stringify(target.area)}), while listening ${JSON.stringify(haloOn)} (box ${JSON.stringify(area)})`;
  t.diagnostic(`halo pixels left of the box: ${halos}`);
  await shot("3-halo");
  assert.ok(haloOn.n > haloBefore.n + 10, `the GL overlay drew no active halo outside the box: ${halos}`);
  // The strip: small, and every former dock action is on it.
  assert.equal(await page.eval(`document.querySelector('.out-strip').hidden`), false);
  const acts = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.out-strip .out button')].map((b) => b.dataset.action))`));
  assert.deepEqual(acts, ["mute", "copy", "stop"], "Mute, Copy address and Stop");
  const stripH = await page.eval(`document.querySelector('.out-strip').getBoundingClientRect().height`);
  assert.ok(stripH < 80, `the strip is a strip, not a bar (${stripH} px)`);
  await page.click(`document.querySelector('.out-strip .out button[data-action=mute]')`);
  await page.waitFor("Mute to take", `document.querySelector('.out-strip .out button[data-action=mute]')?.getAttribute('aria-pressed') === 'true'`, { timeoutMs: 5000 });
  await shot("3-listening");

  // (4) Stop from the same menu → both drop.
  await waitPlaced(page, id, fail);
  const again = await openMenuOn(page, id);
  assert.ok(again.includes("Stop listening"), `the menu offers Stop listening while it plays: ${again}`);
  await page.click(menuItem("Stop listening"));
  await page.frames(2);
  assert.equal(await page.eval(`document.querySelector('.out-strip').hidden`), true, "nothing open: the strip is gone");
  await waitPlaced(page, id, fail);
  assert.equal(await page.eval(`${BADGE(id)} === null`), true, "the badge dropped");
  assert.equal(await page.eval(ACTIVE_BTN(id)), false, "the active state dropped");
  assert.equal(await page.eval(`document.querySelector('.out-strip').hidden`), true, "nothing open: the strip is gone");
  await shot("4-stopped");

  // (5) Decode from the menu → the decode badge while the backend reports it running; drops when done.
  await waitPlaced(page, id, fail);
  const decodeLabels = await openMenuOn(page, id);
  await page.click(menuItemMatching("/^Decode/"));
  const pipe = await page.waitForValue("the backend to report a running decode pipeline on the box",
    `${authed(backend.token, "GET", "/api/pipelines")}.then((r) => JSON.stringify((r.pipelines ?? []).find((p) => p.state === 'running') ?? null))`,
    (v) => typeof v === "string" && v !== "null", { timeoutMs: 30000 });
  if (!pipe.ok) await fail(new Error(`Decode from the box menu started no running pipeline (${pipe.value})`));
  const running = JSON.parse(pipe.value);
  t.diagnostic(`decode: menu ${decodeLabels.find((l) => /^Decode/.test(l))} → pipeline ${running.id} on ${running.emitter_id}`);
  await page.click(`document.querySelector('.mode[data-mode=explore]')`);
  const decId = running.emitter_id ?? id;
  await page.waitFor("the box to carry the decode badge", `/\\bdecode\\b/.test(${KINDS(decId)})`, { timeoutMs: 120000, everyMs: 250 }).catch(fail);
  await shot("5-decoding");
  await page.eval(authed(backend.token, "DELETE", `/api/pipelines/${running.id}`));
  await page.waitFor("the decode badge to drop once the pipeline is done", `${PLACED(decId)} && !/\\bdecode\\b/.test(${KINDS(decId)})`, { timeoutMs: 120000, everyMs: 250 }).catch(fail);

  // (6) 400 px: no dock bar either, and a long-press on a box opens the same menu, on screen.
  await page.conn.send("Emulation.setDeviceMetricsOverride", { width: 400, height: 820, deviceScaleFactor: 1, mobile: false }, page.sessionId);
  await page.conn.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 }, page.sessionId);
  await page.frames(5);
  assert.deepEqual(JSON.parse(await page.eval(BOTTOM_BARS)), [], "a fixed bar still spans the bottom at 400 px");
  const touch = (type, points) => page.conn.send("Input.dispatchTouchEvent", {
    type, touchPoints: points.map(([x, y], i) => ({ x, y, id: i, radiusX: 4, radiusY: 4, force: 1 })),
  }, page.sessionId);
  let opened = false, phoneLabels = [];
  for (let tries = 0; tries < 6 && !opened; tries++) {
    const b = await waitBox(page, "a pressable box at 400 px", { minW: 12, minH: 12, timeoutMs: 60000 }).catch(fail);
    await touch("touchStart", [[b.x, b.y]]);
    await new Promise((r) => setTimeout(r, 700)); // past HOLD_TO_MARK_MS (450 ms): a long-press, not a tap
    await touch("touchEnd", []);
    await page.frames(2);
    opened = await page.eval(`!!document.querySelector('.ctx-menu:not([hidden])')`);
    if (opened) phoneLabels = JSON.parse(await page.eval(MENU_LABELS));
  }
  assert.ok(opened, "a long-press on a box opened no menu at 400 px");
  assert.ok(phoneLabels.includes("Listen") || phoneLabels.includes("Stop listening"), `the long-press menu is the box menu: ${phoneLabels}`);
  const menuRect = JSON.parse(await page.eval(`JSON.stringify(document.querySelector('.ctx-menu').getBoundingClientRect())`));
  assert.ok(menuRect.left >= 0 && menuRect.right <= 400 && menuRect.top >= 0 && menuRect.bottom <= 820, `the menu runs off a 400 px screen: ${JSON.stringify(menuRect)}`);
  await shot("6-phone-longpress");
  await page.key("Escape");

  // Nothing here moved the radio: menus, Listen, badges and gestures are not device actions.
  const device = page.requests.filter((r) => CONTROL.test(r.url) && r.method !== "GET").map((r) => `${r.method} ${r.url}`);
  assert.deepEqual(device, [], "a menu, a badge or a gesture reached a device route");
});
