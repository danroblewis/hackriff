// **T-1009: a Measure box's actions — "Scan this region with <device>"** — over TWO mock front ends
// (CLAUDE.md: e2e drives the system through the device interface; the ticket's own acceptance is a
// two-device run, because choosing the radio is the point).
//
// The ticket's acceptance, end to end in the shipped app:
//
//   draw a box over a grey region → right-click it → choose the SECOND radio's "Scan this region
//   with …" → Start → the scan overlay appears BOUNDED BY THE BOX, on that radio, and coverage
//   fills in there.
//
// What each assertion is a property of:
//
//   1. THE MENU IS THE BOX'S OWN. Right-clicking a measurement box opens the same context menu a
//      detection box gets (T-994), carrying one Scan and one Record-IQ item PER RADIO, by name,
//      plus Save as marker. (A box drawn over grey has no detection under it, so nothing else could
//      have answered the right-click.)
//   2. THE PLAN IS BOUNDED BY THE BOX, ON THE CHOSEN RADIO. Choosing the second radio's Scan item
//      opens the plan overlay whose region is the box's own frequency extent — not the viewport's,
//      not inset — and whose `device_id` is that radio. Nothing has reached a device route yet.
//   3. START COMMISSIONS THAT RADIO, AND COVERAGE FILLS IN. Start posts the priced plan with that
//      `device_id`; the server reports the sweep RUNNING on that radio (`/api/control/state`'s
//      `scans`), its steps tile the box, and the engine's own per-step dwell records appear inside
//      the box — the coverage the map draws, read from the source it is drawn from.
//   Screenshots at 1280×800 and 400 px wide go to the artifacts directory.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(path.dirname(fileURLToPath(import.meta.url)), "artifacts");
const PORT = Number(process.env.HK_E2E_PORT ?? 9216) + 26;

/** Two DIFFERENT recordings: a mock's `device_id` is the recording's own, and `hk serve` refuses two
 * front ends reporting the same one. The second stands in for the user's RTL-SDR. */
const DEVICES = [
  { fixture: "fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta" },
  { fixture: "fixtures/tiny/tone.sigmf-meta", set: { "center-hz": 433900000 } },
];

const MENU_LABELS = `JSON.stringify([...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item .lbl')].map((l) => l.textContent))`;

async function api(backend, urlPath, { method = "GET", body } = {}) {
  const r = await fetch(`${backend.origin}${urlPath}`, {
    method,
    headers: { authorization: `Bearer ${backend.token}`, ...(body ? { "content-type": "application/json" } : {}) },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!r.ok) assert.fail(`${method} ${urlPath} -> ${r.status} ${await r.text()}`);
  return r.json();
}

async function rightClick(page, at) {
  await page.mouse("mouseMoved", at.x, at.y);
  await page.mouse("mousePressed", at.x, at.y, { button: "right", buttons: 2, clickCount: 1 });
  await page.mouse("mouseReleased", at.x, at.y, { button: "right", buttons: 0, clickCount: 1 });
}

/** What the scan overlay draws, as the panel states it. */
const drawn = (page) => page.eval(`(() => { const p = document.querySelector('.map-scan'); const d = p?.dataset ?? {};
  return JSON.stringify({ plan: d.plan ? JSON.parse(d.plan) : null, hidden: !!p?.hidden,
    pane: d.paneF0Hz ? { f0: +d.paneF0Hz, f1: +d.paneF1Hz, left: +d.paneLeftPx, w: +d.paneWPx, top: +d.paneTopPx, h: +d.paneHPx } : null }); })()`);

test("a Measure box scans its own region on the radio the user picks, and coverage fills in there", async (t) => {
  const backend = await startBackend({ port: PORT, devices: DEVICES });
  t.after(async () => {
    await api(backend, "/api/control/scan/stop", { method: "POST", body: {} }).catch(() => {});
    backend.stop();
  });
  const browser = await Browser.open();
  t.after(() => browser.close());

  // The premise, on the wire: two front ends, and one sweep per front end (T-1009).
  const state0 = await api(backend, "/api/control/state");
  const ids = state0.devices.map((d) => d.device_id);
  assert.equal(ids.length, 2, `this run holds two front ends: ${JSON.stringify(ids)}`);
  assert.deepEqual(state0.scans.map((s) => s.device_id), ids, "one sweep per front end, in the same order");
  const second = ids[1];

  const page = await browser.page(undefined, { width: 1280, height: 800 });
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load", "the app page never fired load");
  await page.waitFor("the app shell to mount its surface slot", `!!document.querySelector('.sf-canvas')`, { timeoutMs: 20000 });
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the control-state poll to enable the map's device controls",
    `(() => { const b = document.querySelector('.map-scan-btn'); return !!b && !b.disabled; })()`, { timeoutMs: 20000 });

  // Widen the view so the box can be drawn over spectrum neither radio has looked at (grey).
  for (let i = 0; i < 3; i++) { await page.click("document.querySelector('.map-zoom-out')"); await page.frames(3); }
  await page.frames(10);
  const rect = await page.$rect(".sf-canvas");

  // FREEZE the pane first: a following pane scrolls, and a box anchored in capture time rises up
  // the screen between the drag and the right-click (CLAUDE.md's one shared time axis — the box is
  // laid out in capture time, not at a screen position). Freezing is view state and reaches no
  // device route; it is what makes the pointer coordinates below mean one place.
  await page.click("document.querySelector('.sf-pane-live-btn')");
  await page.frames(4);

  // ---- (1) draw a measurement box, then right-click it ----
  await page.click("document.querySelector('.map-measure-btn')");
  await page.frames(2);
  const y0 = rect.y + rect.h * 0.45, y1 = rect.y + rect.h * 0.65;
  const x0 = rect.x + rect.w * 0.20, x1 = rect.x + rect.w * 0.40;
  await page.drag({ x: x0, y: y0 }, { x: x1, y: y1 }, 10);
  await page.waitFor("the measurement to be saved and drawn",
    `performance.getEntriesByType('resource').some((e) => e.name.includes('/api/measurements'))`, { timeoutMs: 20000 });
  await page.frames(8);
  const measured = await api(backend, "/api/measurements");
  assert.ok(measured.measurements.length > 0, "the drag saved a measurement");
  const m = measured.measurements[0];
  t.diagnostic(`measured ${(m.f_lo_hz / 1e6).toFixed(3)}–${(m.f_hi_hz / 1e6).toFixed(3)} MHz`);
  await page.shot(path.join(ART, "measure-scan-1-box-1280x800.png"));

  const at = { x: (x0 + x1) / 2, y: (y0 + y1) / 2 };
  let labels = null;
  for (let tries = 0; tries < 8 && !labels; tries++) {
    await rightClick(page, at);
    if (await page.eval(`!!document.querySelector('.ctx-menu:not([hidden])')`)) labels = JSON.parse(await page.eval(MENU_LABELS));
    else await new Promise((r) => setTimeout(r, 150));
  }
  assert.ok(labels, "right-clicking the measurement box never opened a menu");
  t.diagnostic(`menu: ${JSON.stringify(labels)}`);
  assert.equal(labels.filter((l) => /^Scan this region with /.test(l)).length, 2, `one Scan item per radio: ${JSON.stringify(labels)}`);
  assert.equal(labels.filter((l) => /^Record IQ of this region with /.test(l)).length, 2, `one Record-IQ item per radio: ${JSON.stringify(labels)}`);
  assert.ok(labels.includes("Save as marker"), `${JSON.stringify(labels)}`);
  await page.shot(path.join(ART, "measure-scan-2-menu-1280x800.png"));

  // ---- (2) choose the SECOND radio's Scan: the plan is bounded by the box, on that radio ----
  const posts0 = page.requests.filter((r) => r.method === "POST").length;
  // The SECOND radio's item — the user's choice, not the default one.
  await page.click(`[...document.querySelectorAll('.ctx-menu:not([hidden]) .ctx-item')]
    .filter((b) => /^Scan this region with /.test(b.querySelector('.lbl')?.textContent ?? ''))[1]`);
  await page.waitFor("the plan to be priced with its steps",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; return !!d && !!JSON.parse(d).windows; })()`, { timeoutMs: 20000 });
  await page.frames(6);
  const opened = JSON.parse(await drawn(page));
  assert.equal(opened.plan.state, "plan");
  assert.equal(opened.plan.device_id, second, "the plan is the radio the user picked");
  const span = m.f_hi_hz - m.f_lo_hz;
  assert.ok(Math.abs(opened.plan.lo_hz - m.f_lo_hz) < span * 0.02, `the plan starts at the box (${opened.plan.lo_hz} vs ${m.f_lo_hz})`);
  assert.ok(Math.abs(opened.plan.hi_hz - m.f_hi_hz) < span * 0.02, `the plan ends at the box (${opened.plan.hi_hz} vs ${m.f_hi_hz})`);
  assert.equal(page.requests.filter((r) => r.method === "POST").length, posts0, "opening a plan from the box posted nothing");
  await page.shot(path.join(ART, "measure-scan-3-plan-1280x800.png"));

  // ---- (3) Start: the chosen radio sweeps the box, and its dwells land inside it ----
  const t0 = Date.now() / 1000;
  await page.click("document.querySelector('.map-scan .map-scan-go')");
  await page.waitFor("the sweep to be running on the chosen radio",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; return !!d && JSON.parse(d).state === 'running'; })()`, { timeoutMs: 20000 });
  const running = JSON.parse(await drawn(page));
  assert.equal(running.plan.device_id, second);
  assert.ok(running.plan.windows.length >= 1, "the running plan draws the engine's own steps");
  assert.ok(running.plan.windows[0][0] >= m.f_lo_hz - span * 0.02 && running.plan.windows.at(-1)[1] <= m.f_hi_hz + span * 0.02,
    `the steps tile the box and nothing else: ${JSON.stringify(running.plan.windows)}`);

  // The server's own word on whose sweep it is: `scans` names the radio, the default one is idle.
  const st = await api(backend, "/api/control/state");
  const mine = st.scans.find((s) => s.device_id === second);
  assert.equal(mine.state, "running", `the chosen radio is the one sweeping: ${JSON.stringify(st.scans.map((s) => [s.device_id, s.state]))}`);
  assert.equal(st.scans.find((s) => s.device_id === ids[0]).state, "idle", "the other radio was not commandeered");

  // Coverage fills in there: the engine's own per-step dwell records, centred on the steps it drew
  // inside the box. This is the source the coverage map is built from (`docs/api.md`: one dwell
  // record per steady tune), read straight off the wire rather than from a colour.
  const centres = running.plan.windows.map((w) => w[2]);
  let dwelt = [];
  const deadline = Date.now() + 90_000;
  for (;;) {
    const obs = await api(backend, `/api/observations?${new URLSearchParams({
      f_lo: String(Math.max(0, m.f_lo_hz - span)), f_hi: String(m.f_hi_hz + span),
      t0: String(t0 - 10), t1: String(Date.now() / 1000 + 1), limit: "1000",
    })}`);
    dwelt = (obs.records ?? []).filter((r) => r.record === "dwell"
      && centres.some((c) => Math.abs((r.window?.center_hz ?? -1) - c) < 1e3));
    if (dwelt.length > 0 || Date.now() > deadline) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  assert.ok(dwelt.length > 0, "the sweep's own dwell records appeared on the steps drawn inside the box — coverage fills in where the plan was");
  await page.shot(path.join(ART, "measure-scan-4-running-1280x800.png"));

  // Stop, from the same button: surrendering that radio is never refused.
  await page.click("document.querySelector('.map-scan-btn')");
  await page.waitFor("the sweep to idle",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; return !d || d === ''; })()`, { timeoutMs: 20000 });

  // The same menu at 400 px, where the map is a phone's.
  const phone = await browser.page(undefined, { width: 400, height: 800, newWindow: true });
  assert.equal(await phone.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await phone.waitFor("the phone shell to mount", `!!document.querySelector('.sf-canvas')`, { timeoutMs: 20000 });
  await phone.waitForSurfaceMounted({ timeoutMs: 60000 });
  await phone.frames(6);
  await phone.shot(path.join(ART, "measure-scan-5-phone-400.png"));
});
