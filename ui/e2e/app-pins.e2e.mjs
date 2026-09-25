// T-809 (MAP-09): pins in the real app over the replayed fixture. The unit tier
// (`ui/test/surface-pins.test.ts`) proves placement, glyph vocabulary, picking and the cap; this
// proves the journey the user sees: a signal blind detection found appears as a PIN on the canvas;
// hovering it shows the MapTip (centre / bandwidth / suggestion / on-air); clicking it selects it
// (a visible selected state) and raises the detail sheet; the pointer over a pin still belongs to
// the surface (so a drag or wheel there still pans/zooms); and none of it reaches a device route.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;

/** The centre of the detection pin nearest the live edge (the top) inside the canvas, this instant.
 * Nearest the top because an ENDED signal's pin rides down with its rows and leaves the pane as
 * they do — honest churn, which a test that grabbed one near the bottom would read as a bug. */
const PIN_AT = `(() => {
  const c = document.querySelector('.sf-canvas').getBoundingClientRect();
  let best = null;
  for (const p of document.querySelectorAll('.sf-pins .sf-pin.detection')) {
    const r = p.getBoundingClientRect();
    const x = r.x + r.width / 2, y = r.y + r.height / 2;
    if (x > c.x + 4 && x < c.right - 4 && y > c.y + 4 && y < c.bottom - 4 && (!best || y < best.y)) best = { id: p.dataset.pin, x, y };
  }
  return best;
})()`;

/** What the page shows, for a failure message: the pins, the listed rows, the pane's readout. */
const STATE = `JSON.stringify({
  pins: [...document.querySelectorAll('.sf-pins .sf-pin')].map((p) => [p.className, p.dataset.pin?.slice(0, 8), p.style.transform]),
  rows: [...document.querySelectorAll('.side-inv .row[data-id]')].map((r) => r.dataset.id.slice(0, 8)),
  chip: document.querySelector('.side-chip')?.textContent ?? null,
  sheet: document.querySelector('.sheet-title')?.textContent ?? null,
  chrome: document.querySelector('.sf-chrome')?.textContent?.slice(0, 300) ?? null,
  note: document.querySelector('.sf-note')?.textContent?.slice(0, 300) ?? null,
})`;

test("a detected signal is a pin: hover shows its MapTip, click selects it and opens its sheet", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  // The app's surface addressed itself, or said why it could not: the surface's own mounted/failed
  // event (T-907: `data-surface` on <html>), which throws with the page's reason on every abort path.
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  const note = (await page.$text(".sf-note")) ?? "";
  assert.ok(!/could not be addressed|WebGL2 is unavailable|busy producing/.test(note), `the app's surface did not mount: ${note}`);
  await page.waitFor("blind detection to put a pin on the canvas", `!!${PIN_AT}`, { timeoutMs: 120000, everyMs: 1000 })
    .catch(async (e) => { throw new Error(`${e.message}\nstate: ${await page.eval(STATE)}`); });

  // Rest: a focusable button with an accessible name saying its kind in words.
  const label = await page.eval(`document.querySelector('.sf-pins .sf-pin.detection').getAttribute('aria-label')`);
  assert.match(label ?? "", /^(confirmed|candidate|unexplained) signal at [\d.]+ MHz$/);

  // The pointer over a pin is the surface's: the pin never takes the gesture from the canvas.
  let at = JSON.parse(await page.eval(`JSON.stringify(${PIN_AT})`));
  assert.equal(await page.eval(`document.elementFromPoint(${at.x}, ${at.y})?.classList.contains('sf-canvas')`), true,
    "the canvas is under a pin, so a drag or wheel there still pans and zooms");

  // Hover → MapTip. A following pane moves the pin as rows arrive, so re-read and re-hover.
  const deadline = Date.now() + 20000;
  let tip = null;
  while (Date.now() < deadline) {
    at = JSON.parse(await page.eval(`JSON.stringify(${PIN_AT})`));
    if (at) {
      await page.mouse("mouseMoved", at.x, at.y);
      await page.frames(2);
      tip = await page.eval(`(() => { const t = document.querySelector('.sf-maptip');
        return t && !t.hidden ? t.textContent : null; })()`);
      if (tip) break;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  assert.ok(tip, "hovering a pin shows its MapTip");
  assert.match(tip, /[\d.]+ MHz/, `the MapTip states the centre: ${tip}`);
  assert.match(tip, /(confirmed|candidate|unexplained) signal/, tip);
  assert.match(tip, /(suggests .+ \(a suggestion, not a finding\)|no explanation yet)/, tip);
  assert.match(tip, /(on air since|ended) /, tip);

  // Click → selected state on the pin, and the detail sheet rises with that signal. Pressed where the
  // pin is NOW (re-read), and retried on a fresh read if that pin left the window in between.
  const SELECTED = `document.querySelector('.sf-pins .sf-pin.selected[aria-pressed="true"]')?.dataset.pin ?? null`;
  let selected = null;
  for (let i = 0; i < 5 && !selected; i++) {
    at = JSON.parse(await page.eval(`JSON.stringify(${PIN_AT})`)) ?? at;
    await page.mouse("mouseMoved", at.x, at.y);
    await page.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
    await page.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
    await page.frames(3);
    selected = await page.eval(SELECTED);
  }
  if (!selected) throw new Error(`no pin showed as selected after clicking; last ${JSON.stringify(at)}; state: ${await page.eval(STATE)}`);
  assert.equal(selected, at.id, "the pin that shows selected is the one clicked");
  await page.waitFor("the sheet to rise with the signal's detail",
    `document.querySelector('.sheet')?.dataset.snap === 'half' && /^Selected signal/.test(document.querySelector('.sheet-title')?.textContent ?? '')`,
    { timeoutMs: 15000 });

  // Moving away hides the tip; the selection stays.
  await page.mouse("mouseMoved", 5, 5);
  await page.frames(2);
  assert.equal(await page.eval(`document.querySelector('.sf-maptip').hidden`), true);
  assert.equal(await page.$count(".sf-pins .sf-pin.selected"), 1);

  assert.deepEqual(page.requests.filter((r) => CONTROL.test(r.url)).map((r) => r.url), [],
    "hovering or selecting a pin reached a device route");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
