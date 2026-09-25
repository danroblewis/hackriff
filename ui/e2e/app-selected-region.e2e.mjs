// **T-943: select a region in the real app, and the region's own panel is what you get.**
//
// The explorer's staging session (2026-09-25, live HackRF in SF) found the sheet titled "Selected
// region" doing two things wrong, and both are browser-level claims the unit tier cannot make:
//
//   (a) it listed signals from OUTSIDE the region (a region at 98.8226–99.0407 MHz headed by
//       107.816 / 106.997 / 106.159 MHz, "Strongest" 106.166 MHz) — because the Explore drawer,
//       which shares that sheet, asked about the whole viewed span. Here the REQUESTS the client
//       builds after the region is committed are read off the wire and checked against the region's
//       own bounds: "a client asking the backend for the wrong thing" is the gap no other gate
//       covers (CLAUDE.md), and a render assertion alone would pass on a server that ignored the
//       band parameters.
//   (b) Listen and Decode were unreachable: the region panel carried no buttons, and what the sheet
//       did carry sat below the fold (Decode RDS at y = 1472 in a 1000 px window). "Below the fold"
//       is exactly what a fake DOM cannot see, so the assertion here is the browser's own hit test
//       at the button's centre — not merely that the element exists.
//
// The gesture itself (shift+drag delivers, and does not pan) is `surface-region.e2e.mjs`'s subject
// and is not re-proved here; this file starts where a committed region does.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

/** One browser and one app page for the file, opened lazily (surface-region.e2e.mjs's pattern). */
let opening = null;
const app = () => (opening ??= openApp());
after(async () => { (await opening?.catch(() => null))?.browser?.close(); });

async function openApp() {
  const browser = await Browser.open();
  const page = await browser.page(undefined, { width: 1280, height: 1000 });
  page.browser = browser;
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load", "the app page never fired load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the app's surface to finish addressing",
    `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
  await page.waitFor("a viewport readout to exist", `document.querySelectorAll('.hk-surface-viewport').length > 0`);
  await page.frames(3);
  return page;
}

/** Regions committed on the wire (never the sidebar, which is window-scoped — T-386). */
const committed = (page) =>
  page.requests.filter((r) => r.method === "POST" && /\/api\/selections$/.test(new URL(r.url).pathname)).length;

/** The browser's own answer to "can the viewer press this?": in the window, and top at its centre. */
const PRESSABLE = (sel) => `(() => {
  const e = document.querySelector(${JSON.stringify(sel)});
  if (!e) return { found: false };
  const r = e.getBoundingClientRect();
  const top = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
  return { found: true, label: (e.textContent ?? '').trim(), y: r.y, bottom: r.bottom,
    inWindow: r.y >= 0 && r.bottom <= window.innerHeight && r.width > 0 && r.height > 0,
    hit: !!top && (top === e || e.contains(top)), coveredBy: top ? (top.className || top.tagName) : 'nothing' };
})()`;

/** The band a query string asks about, in Hz. */
function band(url) {
  const q = new URL(url).searchParams;
  return [Number(q.get("f_lo")), Number(q.get("f_hi"))];
}

test("a region selected on the canvas: only its signals, with Listen and Decode in view", async (t) => {
  const page = await app();
  const r = await page.$rect(".sf-canvas");
  assert.ok(r && r.w > 300 && r.h > 260, `the canvas has no usable box: ${JSON.stringify(r)}`);
  const bottom = r.y + r.h - 130;
  const from = { x: r.x + r.w * 0.35, y: r.y + 60 }, to = { x: r.x + r.w * 0.5, y: bottom - 40 };

  const before = committed(page);
  const marker = page.requests.length;
  await page.drag(from, to, 10, { shift: true });
  for (let i = 0; committed(page) === before && i < 100; i++) await new Promise((k) => setTimeout(k, 100));
  assert.equal(committed(page), before + 1, "the shift+drag did not commit exactly one region");

  // The sheet is about the region now, and says so.
  await page.waitFor("the sheet to name the selected region",
    `/Selected region/.test(document.querySelector('.sheet-title')?.textContent ?? "")`, { timeoutMs: 20000 });

  // (b) Listen and Decode: present, labelled, and pressable where the viewer is looking.
  await page.waitFor("the region panel's actions to render",
    `!!document.querySelector('.focus .actions [data-action="listen-all"]')`, { timeoutMs: 20000 });
  for (const action of ["listen-all", "decode"]) {
    const v = JSON.parse(await page.eval(`JSON.stringify(${PRESSABLE(`.focus .actions [data-action="${action}"]`)})`));
    t.diagnostic(`${action}: ${JSON.stringify(v)}`);
    assert.equal(v.found, true, `no ${action} action on a selected region`);
    assert.match(v.label, action === "decode" ? /Decode/ : /Listen/);
    assert.equal(v.inWindow, true, `${action} is outside the window (y ${v.y}–${v.bottom} of ${await page.eval("window.innerHeight")}) — below the fold, which is how the explorer missed it`);
    assert.equal(v.hit, true, `${action} is covered by ${v.coveredBy}`);
  }

  // (a) the region's own bounds are what the client asks the backend about. The panel states the
  // region in MHz; every band-scoped question asked after the commit must be that band.
  const shown = (await page.$text(".focus .bigf")) ?? "";
  const [lo, hi] = [...shown.matchAll(/[\d.]+/g)].map((m) => Number(m[0]) * 1e6);
  assert.ok(Number.isFinite(lo) && Number.isFinite(hi) && hi > lo, `the panel did not state the region: "${shown}"`);
  const asked = page.requests.slice(marker)
    .filter((q) => /\/api\/(events|analysis\/strongest|scheduler|coverage)/.test(new URL(q.url).pathname))
    .filter((q) => new URL(q.url).searchParams.has("f_lo"));
  assert.ok(asked.length >= 3, `the drawer did not re-ask after the region was selected: ${asked.length} requests`);
  for (const q of asked) {
    const [qlo, qhi] = band(q.url);
    // The panel states MHz to 2 decimals, so the bound it gives is the true one to within 5 kHz;
    // that is the whole slack. The fault this catches is a request about the VIEWED SPAN, which is
    // megahertz away — no tolerance hides it.
    assert.ok(Math.abs(qlo - lo) <= 5100 && Math.abs(qhi - hi) <= 5100,
      `${new URL(q.url).pathname}?${new URL(q.url).search} asked about ${qlo}–${qhi} Hz, not the selected region ${lo}–${hi} Hz`);
  }

  // Every signal the panel lists is inside the region. A loose bound on purpose — which rows the
  // fixture has is not this tier's subject, and a listed row may legitimately straddle an edge by
  // its own bandwidth — but the explorer's rows were 7-9 MHz out, which this catches. The exact
  // predicate (`foundInside`, one row) is pinned in ui/test/app-selected-region.test.ts.
  const listed = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.focus .list .row .f')].map((e) => parseFloat(e.textContent)))`));
  t.diagnostic(`region ${lo / 1e6}–${hi / 1e6} MHz lists ${JSON.stringify(listed)}`);
  for (const mhz of listed) {
    assert.ok(mhz * 1e6 >= lo - 2e6 && mhz * 1e6 <= hi + 2e6,
      `the region panel lists ${mhz} MHz, outside ${lo / 1e6}–${hi / 1e6} MHz`);
  }

  // Selecting, listing and reading a region never commands the radio (T-343: only an explicit
  // device action does) — pressing Listen is not done here, since audio needs a real device path.
  assert.deepEqual(page.requests.filter((q) => CONTROL.test(q.url)).map((q) => q.url), [],
    "selecting a region or opening its panel reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});

test("right-click on a row in the region panel opens its menu (it opened nothing)", async (t) => {
  const page = await app();
  const rows = await page.$count(".focus .list .row[data-id]");
  if (rows === 0) {
    // Honest skip: with no detection inside the region there is no row to right-click. The unit
    // tier (`ui/test/app-selected-region.test.ts`) proves the wiring over a fake DOM; this leg adds
    // the browser's own contextmenu delivery, which needs a row to exist.
    t.diagnostic("no signal inside the region on this fixture: nothing to right-click");
    return;
  }
  const at = JSON.parse(await page.eval(`JSON.stringify((() => {
    const r = document.querySelector('.focus .list .row[data-id]').getBoundingClientRect();
    return { x: r.x + r.width / 2, y: r.y + r.height / 2 };
  })())`));
  await page.mouse("mousePressed", at.x, at.y, { button: "right", buttons: 2, clickCount: 1 });
  await page.mouse("mouseReleased", at.x, at.y, { button: "right", buttons: 0, clickCount: 1 });
  await page.waitFor("the context menu to open on the row",
    `!!document.querySelector('.ctx-menu:not([hidden]) .ctx-item')`, { timeoutMs: 10000 });
  const labels = JSON.parse(await page.eval(`JSON.stringify([...document.querySelectorAll('.ctx-menu .ctx-item')].map((e) => e.textContent.trim()))`));
  t.diagnostic(`menu: ${JSON.stringify(labels)}`);
  assert.ok(labels.some((l) => /^Listen|^Stop listening/.test(l)), `Listen among ${JSON.stringify(labels)}`);
  await page.key("Escape");
  assert.deepEqual(page.requests.filter((q) => CONTROL.test(q.url)).map((q) => q.url), []);
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
