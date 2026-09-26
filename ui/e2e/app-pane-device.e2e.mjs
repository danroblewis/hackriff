// T-1006 (MMAP): **two mock front ends, two panes, two radios** — in the real app, over the
// product's own server, through the device interface (CLAUDE.md: e2e never feeds files into the
// pipeline; `hk serve --device mock:… --device mock:…` is two `hk_core::Source`s behind the same
// driver interface T-513 uses, and the browser drives the whole act).
//
// The gap this closes: MSDR built the whole multi-SDR backend (T-510 N capture sets, T-511 the
// `device_id` selector on every device route, T-512 repeatable `--device`, T-514 the RTL-SDR) and
// the UI had no reader for any of it. A pane carried a `device` nobody could see or set, and a
// retune posted **no** selector — which on a run holding two radios is `400 device_required`,
// because the server refuses to move whichever front end happened to be composed first. Verified by
// hand against this very server before the change:
//
//   POST /api/control/window {"center_hz":101000000,"sample_rate_hz":2000000}
//   → 400 {"code":"device_required","error":"this run holds 2 live front ends, so \"the device\"
//          is not defined: name one with \"device_id\" (…)"}
//
// What this spec asserts, on the acceptance's own terms:
//
//  1. **Both front ends are on the wire**, with different ids and different tuned windows (the
//     premise: without two, everything below would pass vacuously).
//  2. **The pill says whose coverage a pane draws.** On the union with two radios it says so and
//     does not name one of them.
//  3. **"One viewport per front end"** in the viewport menu gives two panes, one pinned to each
//     radio, each stating its own front end — and no device route is touched by taking it.
//  4. **Each pane asks for its OWN radio's coverage**: the `/api/tiles` requests the client actually
//     issued carry `device=<that pane's device_id>` — the grey is per device, from the route's own
//     per-device planes.
//  5. **Each pane's Retune names its own pane's device.** Read off the server's **audit log**, which
//     records `device.id` per device action: the two presses appear as two actions against the two
//     different radios. That is the spy the acceptance asks for, taken on the wire rather than from
//     a client-side stub.
//
// Receive only. Nothing here touches a radio: both front ends are mock SDRs over recordings.
import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const ART = path.join(path.dirname(fileURLToPath(import.meta.url)), "artifacts");
const PANE_ROW = '.hk-surface-viewport[data-viewport="pane"]';

/** Two DIFFERENT recordings, because a mock's `device_id` is the recording's own and `hk serve`
 * refuses two front ends that report the same one. The second is tuned elsewhere with T-512's
 * `--device-set`, so the two radios' coverage — and so the two panes' grey — is genuinely disjoint. */
const DEVICES = [
  { fixture: "fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta" },
  { fixture: "fixtures/tiny/tone.sigmf-meta", set: { "center-hz": 433900000 } },
];

/** Every pane row's device state, read off the element (`data-device`) rather than parsed out of a
 * sentence — the same reason `data-tier` exists: a `device_id` is exactly what prose mangles. */
const PANES = `JSON.stringify([...document.querySelectorAll('${PANE_ROW}')].map((r) => ({
  id: r.querySelector('.hk-surface-id').textContent,
  device: r.dataset.device,
  stale: r.dataset.deviceStale,
  pill: r.querySelector('.hk-surface-device').hidden ? null : r.querySelector('.hk-surface-device').textContent,
  why: r.querySelector('.hk-surface-device').title,
  action: r.querySelector('.hk-surface-action').hidden ? null : {
    label: r.querySelector('.hk-surface-action').textContent,
    enabled: !r.querySelector('.hk-surface-action').disabled,
    why: r.querySelector('.hk-surface-why').textContent,
  },
})))`;

/** Which `device=` values the page's OWN tile requests carried — the requests the browser really
 * made, off the resource timeline, not a client-side spy that could disagree with the network. */
const TILE_DEVICES = `JSON.stringify([...new Set(performance.getEntriesByType('resource')
  .map((e) => e.name).filter((n) => n.includes('/api/tiles'))
  .map((n) => new URL(n).searchParams.get('device')))])`;

/** The device actions the SERVER recorded, in order: what each press actually moved. */
function auditedActions(dataDir) {
  const f = path.join(dataDir, "control-audit.jsonl");
  if (!fs.existsSync(f)) return [];
  return fs.readFileSync(f, "utf8").split("\n").filter(Boolean)
    .map((l) => { try { return JSON.parse(l); } catch { return null; } })
    .filter((e) => e && e.device)
    .map((e) => ({ action: e.device.action, id: e.device.id, status: e.status ?? e.code ?? null }));
}

test("two mock front ends: a device pill per pane, one pane per device, and each retune names its own radio", async (t) => {
  // Its own backend inside this lane's port range (run.mjs hands the lane's base down as
  // HK_E2E_PORT): the shared one is a single front end, which is the case this spec is not about.
  const backend = await startBackend({ port: Number(process.env.HK_E2E_PORT ?? 9216) + 24, devices: DEVICES });
  let browser;
  try {
    // (1) THE PREMISE, on the wire: two front ends, two ids, two tuned windows.
    const state = await (await fetch(`${backend.origin}/api/control/state`, {
      headers: { Authorization: `Bearer ${backend.token}` },
    })).json();
    const ids = state.devices.map((d) => d.device_id);
    t.diagnostic(`front ends: ${JSON.stringify(state.devices.map((d) => [d.device_id, d.tuning.center_hz, d.tuning.sample_rate_hz]))}`);
    assert.equal(ids.length, 2, `this run must hold two front ends, or nothing below is tested: ${JSON.stringify(ids)}`);
    assert.equal(new Set(ids).size, 2, "the two front ends report the same device_id: a selector could not tell them apart");
    assert.equal(state.device, null, "with two radios there is no \"the\" device, and the server must not name one");
    const nav = await (await fetch(`${backend.origin}/api/navigation`, {
      headers: { Authorization: `Bearer ${backend.token}` },
    })).json();
    assert.deepEqual(nav.windows.map((w) => w.device_id).sort(), [...ids].sort(),
      "both front ends must report a capture window, or 'its own coverage' means nothing");

    browser = await Browser.open();
    const page = await browser.page(undefined, { width: 1280, height: 800 });
    assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
    await page.waitForSurfaceMounted({ timeoutMs: 60000 });
    await page.waitFor("a pane row with its device stated",
      `!!document.querySelector('${PANE_ROW}')?.dataset.device`, { timeoutMs: 90000 });
    await page.waitFor("the floating cluster to mount", "!!document.querySelector('.map-pane-btn')", { timeoutMs: 30000 });
    // The device list comes off the 2 s control-state poll, so wait for the page to have it rather
    // than asserting against whatever the first frame happened to know.
    await page.waitFor("the page to know about both front ends",
      `document.querySelector('${PANE_ROW} .hk-surface-device')?.textContent === 'Any of 2'`, { timeoutMs: 30000 });

    // (2) THE PILL. One pane, on the union of two radios: it says so, and names neither.
    const one = JSON.parse(await page.eval(PANES));
    t.diagnostic(`one pane: ${JSON.stringify(one)}`);
    assert.equal(one.length, 1);
    assert.equal(one[0].device, "any");
    assert.equal(one[0].pill, "Any of 2", "the pill must state the union, not a made-up single radio");
    for (const id of ids) assert.ok(one[0].why.includes(id), `the union's sentence does not name ${id}: ${one[0].why}`);
    // …and the retune is refused, out loud, for exactly the reason the route would refuse it.
    assert.equal(one[0].action.enabled, false, "a pane on the union offered a retune that the server must refuse");
    assert.match(one[0].action.why, /pick a front end for it in the viewport menu/);
    await page.shot(path.join(ART, "app-pane-device-1280-union.png"));

    // (3) ONE VIEWPORT PER FRONT END, from the viewport menu, as a user does it.
    await page.click("document.querySelector('.map-pane-btn')");
    await page.waitFor("the viewport menu, with the front-end picker",
      "!document.querySelector('#map-pane-menu').hidden && !document.querySelector('.map-pane-device').hidden", { timeoutMs: 5000 });
    const rows = JSON.parse(await page.eval(
      `JSON.stringify([...document.querySelectorAll('.map-pane-devices input[data-pane-device]')].map((i) => [i.dataset.paneDevice, i.checked]))`));
    t.diagnostic(`picker: ${JSON.stringify(rows)}`);
    assert.deepEqual(rows.map((r) => r[0]), ["any", ...ids], "the picker must offer the union and every attached front end");
    assert.deepEqual(rows.map((r) => r[1]), [true, false, false], "the pane's own choice is marked");
    await page.shot(path.join(ART, "app-pane-device-1280-menu.png"));
    await page.click(`document.querySelector('.map-pane-device [data-pane-act="per-device"]')`);
    await page.waitFor("two panes, one per front end",
      `document.querySelectorAll('${PANE_ROW}').length === 2`, { timeoutMs: 15000 });
    await page.frames(3);
    const two = JSON.parse(await page.eval(PANES));
    t.diagnostic(`two panes: ${JSON.stringify(two)}`);
    assert.deepEqual(two.map((p) => p.device), ids, "each pane must be pinned to its own front end");
    assert.deepEqual(two.map((p) => p.stale), ["false", "false"], "a pane was pinned to a front end this run does not hold");
    for (const p of two) {
      assert.ok(p.pill && p.pill !== "Any of 2", `pane ${p.id} does not name its front end: ${JSON.stringify(p.pill)}`);
      assert.ok(p.why.includes(p.device), `pane ${p.id}'s sentence does not name ${p.device}: ${p.why}`);
    }
    assert.equal(new Set(two.map((p) => p.pill)).size, 2, "two panes on two different radios must not read as the same one");
    // Nothing that has happened so far may have moved a radio: the split and the pick are view state.
    assert.deepEqual(auditedActions(backend.dataDir), [],
      "choosing a front end for a pane reached a device route; it is a view change (docs/16 §8)");
    await page.shot(path.join(ART, "app-pane-device-1280-per-device.png"));

    // (4) EACH PANE'S OWN COVERAGE GREY: the tile requests carry each pane's device, so the grey is
    // that radio's grey — the route's per-device planes, asked for per pane.
    await page.waitFor("both panes' tiles to be asked for by device",
      `(${TILE_DEVICES.replace(/^JSON\.stringify/, "")}).filter((d) => d).length >= 2`, { timeoutMs: 30000 });
    const asked = JSON.parse(await page.eval(TILE_DEVICES));
    t.diagnostic(`tile device= values: ${JSON.stringify(asked)}`);
    for (const id of ids) assert.ok(asked.includes(id), `no tile was requested for ${id}: ${JSON.stringify(asked)}`);

    // (5) EACH PANE'S RETUNE NAMES ITS OWN RADIO. Press both panes' own Retune buttons, then read
    // what the SERVER recorded. A refusal is still an action against a named device — what is under
    // test is WHICH radio each press addressed, not whether that radio accepted the configuration
    // (the second front end's rate is fixed for the run, and says so).
    // The press used is each pane's own **capture-width preset** (T-496) rather than its viewport-
    // covering Retune, and deliberately: both panes open on the OBSERVED extent, which here spans
    // both radios' bands (98–434 MHz), so the viewport-covering plan is correctly refused as survey
    // overview — `span_too_wide`, the honesty rule doing its job. A width preset plans a NAMED span
    // at the pane's own centre, so it is achievable whatever the pane is zoomed to; it reaches the
    // SAME `applyDeviceAction` gate through `paneWidthAction`, and it is the same question: does the
    // press name this pane's radio?
    const widths = `[...document.querySelectorAll('${PANE_ROW}')].map((r) => [...r.querySelectorAll('.hk-surface-width')].find((b) => !b.disabled))`;
    // The status panel is collapsed to one line by default (T-882), and with two panes the second
    // pane's row is clipped out of it — a click at a clipped row's centre lands on whatever is over
    // it. Expand it first, the way a user reading two panes' status does.
    await page.click("document.querySelector('.sf-status-toggle')");
    await page.waitFor("the status detail open", "document.querySelector('.sf-status').dataset.open === 'true'", { timeoutMs: 5000 });
    await page.waitFor("each pane to offer an achievable capture width",
      `${widths}.length === 2 && ${widths}.every(Boolean)`, { timeoutMs: 30000 });
    t.diagnostic(`width presses: ${await page.eval(`JSON.stringify(${widths}.map((b) => [b.textContent, b.title]))`)}`);
    const pressedIds = [];
    for (const i of [0, 1]) {
      // Pressable, then pressed: a control a user cannot reach is not a control (T-528's hit test).
      const covered = await page.eval(`(() => { const e = ${widths}[${i}];
        e.scrollIntoView({ block: "nearest", inline: "nearest" });
        const r = e.getBoundingClientRect();
        const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
        return top === e ? null : String(top && (top.className.baseVal ?? top.className) || top); })()`);
      assert.equal(covered, null, `pane ${i + 1}'s capture-width control is covered by ${covered}`);
      const before = auditedActions(backend.dataDir).length;
      await page.click(`${widths}[${i}]`);
      // One press at a time, waited out on the SERVER's own record — two presses in flight at once
      // can reach the server in either order, and "which radio did pane 2's press move?" is not a
      // question a race may answer.
      for (let k = 0; k < 80 && auditedActions(backend.dataDir).length === before; k++) {
        await new Promise((r) => setTimeout(r, 250));
      }
      const now = auditedActions(backend.dataDir);
      assert.equal(now.length, before + 1, `pane ${i + 1}'s press reached no device route: ${JSON.stringify(now)}`);
      pressedIds.push(now.at(-1).id);
      await page.frames(2);
    }
    t.diagnostic(`audited device actions: ${JSON.stringify(auditedActions(backend.dataDir))}`);
    assert.deepEqual(pressedIds, two.map((p) => p.device),
      "each pane's retune must name ITS OWN front end's device_id");
    // And the pane row's own Retune control is on both panes and states what it would do, whatever
    // the viewport is zoomed to — never absent (T-476: "nothing said is never permissive").
    const rowActions = JSON.parse(await page.eval(
      `JSON.stringify([...document.querySelectorAll('${PANE_ROW} .hk-surface-action:not([hidden])')].map((b) => b.textContent))`));
    assert.deepEqual(rowActions, ["Retune", "Retune"], "a pane lost its retune control");
    await page.shot(path.join(ART, "app-pane-device-1280-retuned.png"));

    // And at a phone width the pill and the picker are still reachable (docs/23: the chrome is the
    // same chrome; the ticket asks for 400 px).
    const narrow = await browser.page(undefined, { width: 400, height: 820 });
    assert.equal(await narrow.goto(`${backend.origin}/#token=${backend.token}`), "load");
    await narrow.waitForSurfaceMounted({ timeoutMs: 60000 });
    await narrow.waitFor("the pill at 400 px",
      `document.querySelector('${PANE_ROW} .hk-surface-device')?.textContent === 'Any of 2'`, { timeoutMs: 60000 });
    await narrow.shot(path.join(ART, "app-pane-device-400.png"));
  } finally {
    if (browser) await browser.close();
    backend.stop();
  }
});
