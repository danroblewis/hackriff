// T-995 (user, 2026-09-25): **the full-spectrum minimap bar is retired.** "If I want to see the whole
// width of the waterfall map, I would zoom or scale to see it, which is how Google Maps works —
// there is never a 'whole world' minimap." In the real app, over the MOCK SDR (a `--replay` backend
// reports no tunable range, so it cannot say what "the full device range" is), at 1280 x 800 and at
// 400 px wide:
//
//   1. NO MINIMAP — no `[data-viewport="minimap"]` row, and the panes run down to the canvas's own
//      bottom inset (nothing is laid out below them).
//   2. ZOOMING OUT REACHES THE WHOLE RANGE — pressing the cluster's Zoom-out, and nothing else,
//      widens the pane until its own readout spans the device range (T-996: the scale block's
//      `data-where`, the frame's own freqLabel — it was the retired row's `.hk-surface-where`).
//   3. THE ACTIVE CAPTURE WINDOW IS STILL ON THE MAP — what only the minimap drew, a lit segment
//      per SDR at the live edge, is now drawn in the zoomed-out pane: its colour (`DEVICE_MARKS[0]`
//      in `surface/minimap.ts`) is found in the pane's newest rows.
//
// Screenshots of both widths, zoomed out, land in the artifacts directory. Nothing here commands
// the radio: zoom is view arithmetic (asserted by the spy tests in `ui/test/app-map-controls.test.ts`).
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");
// The lane's base + 28: inside this lane's 32-port range, clear of the other specs' offsets
// (fog-of-war +8, scan-everything +12, surface-retune +16, ring-drop +20, shadow-level/app-top-chrome +24).
const PORT = Number(process.env.HK_E2E_PORT ?? 8791) + 28;
const CONTROL = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/;
/** `DEVICE_MARKS[0]` (surface/minimap.ts), the first front end's live-segment colour, as 8-bit RGB. */
const DEVICE0 = [0.30, 0.88, 0.60].map((c) => Math.round(c * 255));

/** Up, and the mock SDR has put something in the coverage map — scan-everything.e2e.mjs's readiness. */
async function ready() {
  const be = await startBackend({ port: PORT, mockDevice: true });
  const q = new URLSearchParams({ f_lo: "1000000", f_hi: "6000000000", cells: "128", rows: "8" });
  for (const t0 = Date.now(); ;) {
    const r = await fetch(`${be.origin}/api/coverage?${q}`, { headers: { authorization: `Bearer ${be.token}` } }).catch(() => null);
    const cov = r?.ok ? await r.json() : null;
    if ((cov?.any?.cells ?? []).some((c) => c?.state === "observed")) return be;
    if (Date.now() - t0 > 60000) throw new Error("the mock SDR put nothing in the coverage map in 60 s");
    await new Promise((res) => setTimeout(res, 500));
  }
}
/** Widths at which the segment's spot was the visible surface AND lit. */
const litAt = [];
let backendP = null;
const backend = () => (backendP ??= ready());
after(async () => { (await backendP?.catch(() => null))?.stop(); });

/** The pane's frequency window, parsed from the scale block's `data-where` (canvas-journey.e2e.mjs's reading since T-996). */
function windowOf(where) {
  const m = /^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/.exec(where);
  assert.ok(m, `the pane readout is not a frequency window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { loHz: centerHz - halfHz, hiHz: centerHz + halfHz, spanHz: 2 * halfHz };
}
const WHERE = `document.querySelector('.sf-scale')?.dataset.where ?? ""`;

for (const [width, height] of [[1280, 800], [400, 800]]) test(`at ${width} x ${height} there is no minimap, and zooming out reaches the whole device range with the live capture window lit`, async (t) => {
  const be = await backend();
  const nav = await (await fetch(`${be.origin}/api/navigation`, { headers: { authorization: `Bearer ${be.token}` } })).json();
  const grid = nav.frequency;
  const tuned = nav.windows?.[0];
  assert.ok(grid && tuned, `the mock reported no frequency grid or capture window: ${JSON.stringify(nav).slice(0, 400)}`);
  // The front end's own tunable range (`frequency.ranges_hz`, docs/api.md) — never a constant.
  const ranges = grid.ranges_hz ?? [];
  assert.ok(ranges.length > 0, `the mock states no tunable range: ${JSON.stringify(grid)}`);
  const devLo = Math.min(...ranges.map((r) => r[0])), devHi = Math.max(...ranges.map((r) => r[1]));

  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { width, height });
  assert.equal(await page.goto(`${be.origin}/#token=${be.token}`), "load");
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the pane readout and the zoom control",
    `(${WHERE}).length > 0 && !!document.querySelector('.map-zoom-out')`, { timeoutMs: 30000 });
  await page.frames(3);

  // 1. No minimap. T-996 retired the per-viewport rows from the app altogether, so no viewport row
  // of either kind is drawn; a pane states itself by its own scale block, one per pane.
  assert.equal(await page.$count('.hk-surface-viewport[data-viewport="minimap"]'), 0,
    `a minimap viewport row is still drawn at ${width} px`);
  assert.equal(await page.$count('.sf-scale'), 1, "the page draws no pane (or more than one)");

  // 2. Zoom out with the cluster's own control until the pane spans the device range.
  const before = windowOf(await page.eval(WHERE));
  let w = before;
  for (let i = 0; i < 40 && w.spanHz < 0.99 * (devHi - devLo); i++) {
    await page.click("document.querySelector('.map-zoom-out')");
    await page.frames(2);
    w = windowOf(await page.eval(WHERE));
  }
  t.diagnostic(`at ${width} px the pane went ${(before.spanHz / 1e6).toFixed(3)} MHz -> ${(w.spanHz / 1e6).toFixed(1)} MHz ` +
    `(${(w.loHz / 1e6).toFixed(1)}-${(w.hiHz / 1e6).toFixed(1)} MHz); device ${(devLo / 1e6).toFixed(1)}-${(devHi / 1e6).toFixed(1)} MHz`);
  assert.ok(w.spanHz >= 0.99 * (devHi - devLo),
    `zooming out never reached the whole device range: ${w.loHz}-${w.hiHz} Hz of ${devLo}-${devHi} Hz`);
  // The readout states 3 significant figures at GHz, so the ends are known to ~5 MHz.
  assert.ok(w.loHz <= devLo + 10e6 && w.hiHz >= devHi - 10e6, `the zoomed-out pane is not the device range: ${w.loHz}-${w.hiHz}`);
  assert.equal(await page.eval(`document.querySelector('.sf-scale').dataset.following`),
    "true", "zooming out walked the pane off the live edge, so there is no live edge to light");

  // 3. The mock's active capture window, lit at the pane's live edge.
  await page.frames(6);
  const shot = await page.shot(path.join(ART, `app-no-minimap-${width}x${height}.png`));
  const rect = await page.$rect(".sf-canvas");
  const dpr = await page.eval("window.devicePixelRatio || 1");
  const ins = await page.canvasInsets();
  // T-1041: the pane's first row is the inset itself — the trace reserves nothing.
  const top = Math.round((rect.y + ins.top) * dpr);
  // Where the segment must be: the tuned window's centre through the pane's own frequency mapping
  // (the readout `w` the zoom-out loop ended on), +-8 CSS px for the 2 px drawing floor and rounding.
  const mid = (tuned.f_lo_hz + tuned.f_hi_hz) / 2;
  const xCss = rect.x + ((mid - w.loHz) / (w.hiHz - w.loHz)) * rect.w;
  const x0 = Math.max(0, Math.floor((xCss - 8) * dpr)), x1 = Math.min(shot.width, Math.ceil((xCss + 8) * dpr));
  // Is that spot the SURFACE, or floating chrome over it? The browser's own hit test, under the
  // harness's `unoccludedColumns` rule (the surface's mount counts; `data-band="chrome"` does not).
  // A pixel claim is made only where the surface is visible — a chip over the corner is not a
  // missing segment — and the file's last test requires the claim to have been made at SOME width.
  const visible = await page.eval(`(() => {
    const c = document.querySelector('.sf-canvas'); const mount = c.closest('.surface') ?? c.parentElement;
    const el = document.elementFromPoint(${xCss}, ${(top + 1) / dpr});
    return !!el && mount.contains(el) && !el.closest('[data-band="chrome"]'); })()`);
  let lit = 0;
  for (let y = Math.max(0, top - 3); y < Math.min(shot.height, top + 12); y++) {
    for (let x = x0; x < x1; x++) {
      const d = (y * shot.width + x) * 4;
      if (Math.abs(shot.data[d] - DEVICE0[0]) + Math.abs(shot.data[d + 1] - DEVICE0[1]) + Math.abs(shot.data[d + 2] - DEVICE0[2]) <= 40) lit++;
    }
  }
  t.diagnostic(`at ${width} px, ${lit} px of the live-segment colour at x ${Math.round(xCss)} CSS px in the pane's newest rows ` +
    `(y ${top - 3}-${top + 12} device px); that spot is ${visible ? "the surface" : "UNDER floating chrome"}; ` +
    `tuned ${(tuned.f_lo_hz / 1e6).toFixed(1)}-${(tuned.f_hi_hz / 1e6).toFixed(1)} MHz`);
  if (visible) {
    assert.ok(lit >= 2, "the mock's active capture window is not lit on the zoomed-out pane — the minimap's segment was lost, not moved");
    litAt.push(width);
  }

  const control = page.requests.filter((r) => CONTROL.test(r.url) && r.method !== "GET");
  assert.deepEqual(control.map((r) => r.url), [], "zooming out reached a device route");
});

test("the live capture segment was seen lit on the surface at one width at least (not vacuously skipped everywhere)", () => {
  assert.ok(litAt.length > 0, "at every width the segment's spot was under floating chrome, so nothing proved it is drawn");
});
