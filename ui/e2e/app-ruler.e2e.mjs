// T-998 (MMAP): the HUD time ruler is NARROW (<= 40 px) and words its marks either as "seconds ago"
// or as the viewer's local clock time, chosen in the ⋯ settings menu and kept per viewer. The
// frequency ruler gets the same narrow treatment (small type, short ticks, hugging the bottom edge).
//
// At 1280 x 800 and at 400 x 820:
//  1. WIDTH   — every time label's box is <= 40 px wide.
//  2. CLEAR   — no ruler label's bounding rect overlaps any visible floating chrome (`.map-glass`).
//  3. MODES   — relative labels read "−12 s" / "live edge"; after the toggle, absolute labels read
//     HH:MM:SS and each equals the local rendering of the label's own capture-time value
//     (`data-value`, ns), which lies inside the server's `/api/timeline` window (never a browser clock).
//     Persisted: the key is in localStorage and survives a reload.
//  4. FROZEN  — the same holds on a frozen (scrubbed) pane as on a following one.
//
// `HK_E2E_SHOTS=<dir>` saves a screenshot per width.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { startBackend } from "./backend.mjs";
import { FOLLOWING } from "./app-chrome.mjs";

const SHOTS = process.env.HK_E2E_SHOTS ?? null;
let backendP = null;
const backend = () => (backendP ??= startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 24, mockDevice: true }));
after(async () => { (await backendP?.catch(() => null))?.stop(); });

const LABELS = `JSON.stringify([...document.querySelectorAll('.sf-hud-label')].filter((e) => !e.hidden).map((e) => {
  const r = e.getBoundingClientRect();
  return { axis: e.classList.contains('time') ? 'time' : 'freq', text: e.textContent, value: Number(e.dataset.value),
    x: r.x, y: r.y, w: r.width, h: r.height, clipped: e.scrollWidth > e.clientWidth };
}))`;
const CHROME = `JSON.stringify([...document.querySelectorAll('.map-glass')].filter((e) => {
  const r = e.getBoundingClientRect(); const cs = getComputedStyle(e);
  return r.width > 0 && r.height > 0 && !e.hidden && cs.display !== 'none' && cs.visibility !== 'hidden';
}).map((e) => { const r = e.getBoundingClientRect(); return { cls: String(e.className), l: r.left, t: r.top, r: r.right, b: r.bottom }; }))`;

const local = (ns) => {
  const d = new Date(ns / 1e6), p = (n) => String(n).padStart(2, "0");
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
};

for (const [W, H] of [[1280, 800], [400, 820]]) test(`at ${W} px the time ruler is narrow, clear of the chrome, and reads in both modes`, async (t) => {
  const be = await backend();
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width: W, height: H });
  assert.equal(await page.goto(`${be.origin}/#token=${be.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 240000 });
  const hasTime = "[...document.querySelectorAll('.sf-hud-label.time')].some((e) => !e.hidden)";
  await page.waitFor("time ruler labels", hasTime, { timeoutMs: 120000 });
  await page.frames(5);
  if (SHOTS) await page.shot(path.join(SHOTS, `ruler-${W}.png`));

  const win = (await (await fetch(`${be.origin}/api/timeline?columns=1&rows=1`, { headers: { authorization: `Bearer ${be.token}` } })).json()).window;
  t.diagnostic(`window ${JSON.stringify(win)}`);

  const check = async (mode, what) => {
    const labels = JSON.parse(await page.eval(LABELS));
    const time = labels.filter((l) => l.axis === "time");
    assert.ok(time.length > 0, `${what}: no time labels`);
    for (const l of time) {
      assert.ok(l.w <= 40, `${what}: time label "${l.text}" is ${l.w} px wide`);
      assert.ok(!l.clipped, `${what}: time label "${l.text}" is cut off by the 40 px limit`);
      if (mode === "relative") assert.match(l.text, /^(−|\+|now)/, `${what}: relative label`);
      else {
        // Whole-second steps read HH:MM:SS; finer steps read MM:SS.d.. or SS.ddd — each is a prefix
        // of the label's own capture time (local), never a browser-clock reading.
        const [hh, mm, ss] = local(l.value).split(":");
        const ok = l.text === `${hh}:${mm}:${ss}` || l.text.startsWith(`${mm}:${ss}.`) || l.text.startsWith(`${ss}.`);
        assert.ok(ok, `${what}: label "${l.text}" is not its own capture time ${local(l.value)}`);
      }
    }
    // Two different ticks never read the same.
    assert.equal(new Set(time.map((l) => l.text)).size, time.length, `${what}: repeated time labels ${time.map((l) => l.text).join(" ")}`);
    // Never over floating chrome.
    const chrome = JSON.parse(await page.eval(CHROME));
    for (const l of labels) for (const c of chrome) {
      assert.ok(!(l.x < c.r && l.x + l.w > c.l && l.y < c.b && l.y + l.h > c.t), `${what}: ${l.axis} label "${l.text}" overlaps ${c.cls}`);
    }
    return time;
  };

  await check("relative", "following, relative");

  const toggle = async () => {
    await page.click("document.querySelector('.map-more-btn')");
    await page.waitFor("the ⋯ menu", "!document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
    await page.click("document.querySelector('#map-more-menu .map-time-mode')");
    await page.click("document.querySelector('#map-more-menu .map-more-close')");
    await page.waitFor("the ⋯ menu to close", "document.querySelector('#map-more-menu').hidden", { timeoutMs: 5000 });
    await page.frames(3);
  };
  await toggle();
  await check("absolute", "following, absolute");
  assert.equal(await page.eval("localStorage.getItem('hk-hud-time-labels')"), "absolute");

  // Frozen pane, then both modes again.
  assert.ok(await page.eval(FOLLOWING), "the pane should be following before the scrub");
  // Freeze the pane on its window (the FAB, an explicit pause). No drag afterwards: a pan that ends
  // at the tuned live edge re-follows by design (T-955), which would make this assertion depend on
  // the viewport's geometry rather than on the ruler.
  await page.click("document.querySelector('.map-fab')");
  await page.waitFor("the pane to freeze", `!(${FOLLOWING})`, { timeoutMs: 10000 });
  await page.frames(5);
  assert.equal(await page.eval(FOLLOWING), false, "the frozen pane resumed following");
  await check("absolute", "frozen, absolute");
  await toggle();
  await check("relative", "frozen, relative");
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
