// T-1007 (MMAP): the ⋯ settings menu replaces most of Review. User, 2026-09-25 on staging (via the
// supervisor): "instead of the Review section a lot of those things could be considered settings."
//
// What each assertion is a property of, at 1280 x 800 and at 400 x 820:
//  1. ONE SMALL BUTTON — the settings menu is the cluster's ⋯ (docs/23 §10 P4: small controls for big
//     view changes), no bigger than its neighbours, and a single click opens it.
//  2. EVERY MOVED SETTING WORKS — each is clicked for real and its effect is read off the PAGE, never
//     off the control that was pressed: Theme (the document's theme), the colour scale (the surface's
//     own range sentence, and the stored preference), the time ruler (a HUD time label turns from
//     seconds-ago into a UTC timestamp), the front ends (the pane's readout names the radio whose
//     coverage it reads, and stops naming it again), the capture window (the retention the backend
//     reports, in words), and the three bigger panels (they open in the drawer).
//  3. NOTHING CONFIGURABLE IS LEFT IN REVIEW — pressing Review shows the review group only: Alarms and
//     the survey report. No Device, no Scheduler, no Bookmarks tab is reachable there, and the drawer
//     names the group it is showing.
//  4. THE VIEW/DEVICE LINE — none of it reaches a device route (the spy-client rule, as requests).
//
// `HK_E2E_SHOTS=<dir>` saves a screenshot per width, plus one with the menu open.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";

const SHOTS = process.env.HK_E2E_SHOTS ?? null;
// The DEVICE ACTIONS only: `/api/control/state` and the sweep's price preview are reads, and the
// device panel this menu opens is allowed to read them.
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)\b/;

// Its own backend on the MOCK device (lane base + 16, the offset the other second-backend specs
// use), so `/api/control/state` reports a live front end and the front-end list has a radio in it.
let backendP = null;
const backend = () => (backendP ??= startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 16, mockDevice: true }));
after(async () => { (await backendP?.catch(() => null))?.stop(); });

/** The visible tabs of the drawer, as labels — what a person can actually reach there. */
const VISIBLE_TABS = `JSON.stringify([...document.querySelectorAll('#review .rv-tab')]
  .filter((b) => !b.hidden && b.getBoundingClientRect().height > 0).map((b) => b.textContent))`;

/** One pane's readout headline (frequency · time · the device whose coverage it reads). */
// T-996 retired the per-viewport panel; the active pane's one-line readout (`.sf-where`) is where a
// pane names the front end whose coverage decides its grey.
const HEADLINE = `(document.querySelector('.sf-where')?.textContent ?? "")`;

for (const [W, H] of [[1280, 800], [400, 820]]) test(`at ${W} px one small ⋯ holds every moved setting, and Review keeps only review`, async (t) => {
  const be = await backend();
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: W, height: H });
  assert.equal(await page.goto(`${be.origin}/#token=${be.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 240000 });
  await page.waitFor("the floating cluster to be up", "!!document.querySelector('.map-topright .map-more-btn')", { timeoutMs: 120000 });
  await page.frames(5);
  if (SHOTS) await page.shot(path.join(SHOTS, `settings-${W}.png`));

  // ---- (1) one small button ----
  const more = await page.$rect(".map-more-btn");
  const layers = await page.$rect(".map-layers-btn");
  t.diagnostic(`at ${W}: ⋯ ${JSON.stringify(more)} vs layers ${JSON.stringify(layers)}`);
  assert.ok(more.w >= 24 && more.h >= 24, `the ⋯ button is smaller than a touch target: ${JSON.stringify(more)}`);
  assert.ok(more.w <= 44 && more.h <= 44, `the ⋯ button is not small: ${JSON.stringify(more)}`);
  assert.ok(more.w <= layers.w + 1, "the ⋯ button is bigger than the cluster's other buttons");

  await page.click("document.querySelector('.map-more-btn')");
  await page.waitFor("the settings menu to open", "!document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
  if (SHOTS) await page.shot(path.join(SHOTS, `settings-${W}-open.png`));
  // Every group the ticket names, in the one menu.
  for (const axis of ["scale", "ruler", "devices", "panels", "capture"]) {
    assert.equal(await page.$count(`#map-more-menu [data-axis="${axis}"]`), 1, `the settings menu has no ${axis} group`);
  }
  assert.equal(await page.$count("#map-more-menu #theme-btn"), 1, "Theme is not in the settings menu");
  // It is a MENU, not a page: it fits on screen and scrolls rather than running off the bottom.
  const box = await page.$rect("#map-more-menu");
  assert.ok(box.y >= 0 && box.y + box.h <= H + 1, `the settings menu runs off the screen: ${JSON.stringify(box)} at ${W}x${H}`);

  // ---- (2) every moved setting, clicked, read off the page ----

  // Theme: the document's own theme stamp changes (shell.ts's `data-theme`), not just the label.
  const theme0 = await page.eval("String(document.documentElement.dataset.theme ?? 'system')");
  await page.click("document.querySelector('#map-more-menu #theme-btn')");
  await page.waitFor("the theme to change", `String(document.documentElement.dataset.theme ?? 'system') !== ${JSON.stringify(theme0)}`, { timeoutMs: 5000 });

  // Colour scale (= auto-contrast): the surface's own range sentence follows the mode, and the
  // preference is stored. Clicked through the radio the menu offers, never by calling the surface.
  const note0 = await page.$text("#map-more-menu .sf-range-note");
  await page.click("document.querySelector('#map-more-menu [data-scale=\"viewport\"]')");
  await page.waitFor("the colour scale to become viewport-dynamic",
    `localStorage.getItem('hk-surface-range-mode') === 'viewport'
     && !!document.querySelector('#map-more-menu [data-scale="viewport"]').checked`, { timeoutMs: 10000 });
  await page.waitFor("the range sentence to state the new scale",
    `(document.querySelector('#map-more-menu .sf-range-note')?.textContent ?? "") !== ${JSON.stringify(note0)}
     || /viewport/.test(document.querySelector('#map-more-menu .sf-range-note')?.textContent ?? "")`, { timeoutMs: 15000 });
  await page.click("document.querySelector('#map-more-menu [data-scale=\"anchored\"]')");
  await page.waitFor("the anchored scale to come back", "localStorage.getItem('hk-surface-range-mode') === 'anchored'", { timeoutMs: 10000 });

  // The time ruler. TWO properties, because at phone width there may be nothing on screen to read:
  //   - the mode is taken and remembered (the page's own stored preference), at every width;
  //   - where the HUD actually prints time labels, they turn from "-1m20s" into a clock time,
  //     ALL of them, on the same marks. The mode is T-998's (`hk-hud-time-labels`, local clock in the
  //     narrow ruler's forms: HH:MM:SS, or MM:SS.d / SS.ddd under a second).
  // T-997 drops a time label that would print into the map's top-left floating chrome, and at 400 px
  // that column is the whole width — so there the ruler legitimately shows none, and this says so
  // out loud rather than asserting on the hidden pool elements (asserting on those is what made an
  // earlier draft of this spec look green at 1280 and red at 400 for the wrong reason).
  const shownTimeLabels = `JSON.stringify([...document.querySelectorAll('.sf-hud-label.time')]
    .filter((e) => !e.hidden).map((e) => e.children[0].textContent))`;
  const ageLabels = JSON.parse(await page.eval(shownTimeLabels));
  t.diagnostic(`at ${W}: time labels before the switch ${JSON.stringify(ageLabels)}`);
  await page.click("document.querySelector('#map-more-menu [data-ruler=\"absolute\"]')");
  await page.waitFor("the clock-time mode to be taken and remembered",
    "localStorage.getItem('hk-hud-time-labels') === 'absolute'", { timeoutMs: 10000 });
  if (ageLabels.length) {
    assert.ok(ageLabels.every((s) => /(s|m|h)\b|^now$/.test(String(s))), `not seconds-ago to start from: ${JSON.stringify(ageLabels)}`);
    await page.waitFor("every drawn time label to read a clock time",
      `(() => { const t = [...document.querySelectorAll('.sf-hud-label.time')].filter((e) => !e.hidden)
         .map((e) => e.children[0].textContent);
         return t.length > 0 && t.every((s) => /^(\\d\\d:\\d\\d:\\d\\d|\\d\\d:\\d\\d\\.\\d{1,2}|\\d\\d\\.\\d{3})$/.test(String(s))); })()`,
      { timeoutMs: 30000 });
  } else {
    // Nothing is drawn: prove that is T-997's reserve (every pooled label hidden), not a broken HUD.
    assert.equal(await page.eval("[...document.querySelectorAll('.sf-hud-label.time')].every((e) => e.hidden)"), true,
      "no time label is shown and the HUD is not reporting them as dropped — the ruler is simply missing");
    t.diagnostic(`at ${W}: the time ruler prints no label here (T-997 drops labels under the top-left chrome), so the mode is asserted as taken, not as drawn`);
  }
  await page.click("document.querySelector('#map-more-menu [data-ruler=\"relative\"]')");
  await page.waitFor("the seconds-ago labels to come back", "localStorage.getItem('hk-hud-time-labels') === 'relative'", { timeoutMs: 10000 });

  // The front ends: the live radio is listed, and choosing it makes the pane's readout say whose
  // coverage decides its grey. Choosing "Any front end" takes the name back off.
  await page.waitFor("the front-end list to name the live radio",
    "document.querySelectorAll('#map-more-menu .map-device').length >= 1 && document.querySelectorAll('#map-more-menu select[data-pane-device]').length >= 1",
    { timeoutMs: 30000 });
  const devId = await page.eval(`(() => { const o = [...document.querySelectorAll('#map-more-menu select[data-pane-device] option')]
    .find((x) => x.value !== 'any'); return o ? o.value : null; })()`);
  assert.ok(devId, "the settings menu offers no front end to read coverage from");
  const pick = (v) => page.eval(`(() => { const s = document.querySelector('#map-more-menu select[data-pane-device]');
    s.value = ${JSON.stringify(v)}; s.dispatchEvent(new Event('change', { bubbles: true })); return s.value; })()`);
  assert.equal(await pick(devId), devId);
  await page.waitFor("the pane readout to name the front end it reads coverage from",
    `${HEADLINE}.includes(${JSON.stringify(devId)})`, { timeoutMs: 30000 });
  assert.equal(await pick("any"), "any");
  await page.waitFor("the readout to stop naming one front end",
    `!${HEADLINE}.includes(${JSON.stringify(devId)})`, { timeoutMs: 30000 });

  // The capture window: the retention this server is configured with, in words — never a placeholder.
  const retention = await page.$text("#map-more-menu .map-capture-retention");
  t.diagnostic(`at ${W}: retention reads "${retention}"`);
  assert.match(String(retention), /^\d+(\.\d+)?\s(s|min|h)$/, "the capture window is not stated as a duration");

  // The three bigger panels open in the drawer, and the drawer says it is Settings.
  await page.click("document.querySelector('#map-more-menu [data-panel=\"device\"]')");
  await page.waitFor("the device panel to open as a setting",
    `!document.querySelector('#review').hidden
     && document.querySelector('#review .section-h')?.textContent === 'Settings'
     && !!document.querySelector('#review .rv-tab[aria-selected="true"]')`, { timeoutMs: 10000 });
  assert.deepEqual(JSON.parse(await page.eval(VISIBLE_TABS)), ["Device & display", "Scheduler", "Bookmarks"],
    "the settings drawer does not show exactly the three moved panels");
  assert.equal(await page.eval("document.querySelector('#map-more-menu').hidden"), true, "the ⋯ menu stayed open under the panel it opened");

  // ---- (3) Review keeps only review ----
  // The drawer's own Close first: at phone width it covers the map (and with it the cluster), so
  // reaching Review from an open drawer is a close and a press, exactly as a person would do it.
  await page.click("document.querySelector('#review .rv-head button')");
  await page.waitFor("the drawer to close", "document.querySelector('#review').hidden", { timeoutMs: 5000 });
  await page.click("document.querySelector('.map-topright #review-btn')");
  await page.waitFor("Review to show the review group",
    "document.querySelector('#review .section-h')?.textContent === 'Review'", { timeoutMs: 10000 });
  assert.deepEqual(JSON.parse(await page.eval(VISIBLE_TABS)), ["Alarms", "Survey report"],
    "something configurable is still reachable in Review");
  // And no configurable control is left in the drawer's review group: no gain slider, no sweep.
  assert.equal(await page.$count("#review .rv-tabpanel:not([hidden]) input[type=range], #review .rv-tabpanel:not([hidden]) fieldset"), 0,
    "a device/display fieldset or gain slider is still shown under Review");

  // ---- (4) nothing above reached a device route ----
  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "a setting reached a device route");
});
