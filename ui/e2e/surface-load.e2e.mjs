// **T-450's guard**: the renderer must load, and draw, in a real browser under the product's own
// Content-Security-Policy (T-455).
//
// The defect this file exists for: `cellrule.ts` compiled its pattern predicates with `new
// Function` at module scope. `hk serve` sends `default-src 'self'` with no `unsafe-eval`, so the
// module threw **while being evaluated** — the bundle never finished, nothing mounted, the page was
// blank. Every suite was green, including T-441's, which had proved that same module's shader
// against a CPU rule on 114 973 of 115 200 pixels. A correct proof about code that could never run
// where the product runs.
//
// So the assertions here are, in order of what they would have caught:
//   1. no `securitypolicyviolation` event — the exact mechanism, named by directive;
//   2. no uncaught exception during load;
//   3. the page mounted: the failure card is absent and the slots hold their real content;
//   4. **it drew**: a histogram over the canvas's own rectangle in a screenshot of the composited
//      page. Not a golden image — a golden would fail on font rendering and be switched off — but
//      the same kind of claim T-441 makes one tier down: how many distinct colours, how flat, how
//      bright. A page that loaded and mounted but drew nothing is a flat fill, and fails (3) never
//      catches that while (4) always does.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

test("GET /surface.html loads, mounts and draws under the product CSP", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();
  const tiles = page.watchConcurrency("tiles", (u) => u.includes("/api/tiles"));

  const t0 = Date.now();
  assert.equal(await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`), "load", "the page never fired load");

  // (1) The mechanism, named. A violation of `script-src`/`default-src` is T-450 exactly.
  const violations = await page.eval("JSON.stringify(window.__cspViolations ?? [])");
  assert.equal(violations, "[]", `the page violated its own CSP: ${violations}`);

  // (2) Nothing threw on the way up. `new Function` at module scope shows here as an EvalError
  // whose message names the CSP, but ANY uncaught exception during load is a failure to mount.
  assert.deepEqual(page.exceptions, [], "uncaught exception during load");

  // (3) It mounted. `.sp-fail` is the card `preview-main.ts` swaps in for every abort path, so its
  // absence is the positive statement that none of them was taken.
  await page.waitForSurfaceMounted();
  assert.equal(await page.$count(".sp-fail"), 0, `failure card shown: ${await page.$text(".sp-fail")}`);
  assert.ok(await page.$count(".sp-legend-row") >= 5,
    "the legend is generated from cellrule.ts's tables — no rows means that module did not load");

  // The page's own claim about what the backend said it observed. `0 observed` would mean the
  // surface is legitimately all-grey, and then the pixel census below could not tell "drew grey
  // honestly" from "drew nothing" — so this is established before it, not after. It is a wait
  // rather than a read because a cold backend has ingested no history yet.
  await page.waitFor("the backend to report observed coverage",
    `(document.querySelector('[data-slot="census"]')?.textContent?.match(/(\\d+) observed/)?.[1] | 0) > 0`,
    { timeoutMs: 90000 });
  assert.match((await page.$text('[data-slot="edge"]')) ?? "", /newest recorded/);

  // (4) It drew. First: a drawing buffer that matches the element's CSS box at this dpr — a canvas
  // sized 0, or sized to the wrong box, renders "successfully" into nothing (the T-443 standard:
  // assert the size, in device pixels).
  const rect0 = await page.$rect('[data-slot="canvas"]');
  assert.ok(rect0 && rect0.w > 200 && rect0.h > 200, `canvas has no box: ${JSON.stringify(rect0)}`);
  const buf = await page.eval(`(() => { const c = document.querySelector('[data-slot="canvas"]');
    return { w: c.width, h: c.height, dpr: window.devicePixelRatio }; })()`);
  assert.equal(buf.w, Math.round(rect0.w * buf.dpr), "drawing-buffer width is not the CSS box at this dpr");
  assert.equal(buf.h, Math.round(rect0.h * buf.dpr), "drawing-buffer height is not the CSS box at this dpr");

  // Then the histogram. `drawn` is the shape of the claim: enough distinct colours that this is a
  // ramp and not a clear, no single colour owning the frame, and not black.
  const drawn = (c) => c.distinct >= 32 && c.dominantShare < 0.92 && c.meanLuma > 8;
  const { census: c, rect, ms } = await page.waitForCanvas('[data-slot="canvas"]', drawn,
    { timeoutMs: 90000, saveAs: path.join(ART, "surface-load.png") });
  t.diagnostic(`canvas ${rect.w}×${rect.h} drawn after ${ms} ms: ${c.distinct} distinct colours, ` +
    `dominant ${c.dominant} at ${(c.dominantShare * 100).toFixed(1)} %, mean luma ${c.meanLuma.toFixed(1)}`);

  // What was requested, not only what came back (the T-442 standard). The page is read-only over
  // recorded history: it may address tiles, navigation and coverage, and NOTHING that moves a radio.
  const tileReqs = page.requests.filter((r) => r.url.includes("/api/tiles"));
  assert.ok(tileReqs.some((r) => r.status === 200), "no /api/tiles request was answered 200");
  assert.ok(tiles.peak > 0, "no tile request was ever in flight");
  const control = page.requests.filter((r) => /\/api\/control\//.test(r.url));
  assert.deepEqual(control, [], "the preview reached a control route — this page must never command the front end");

  t.diagnostic(`load-to-drawn ${Date.now() - t0} ms · ${page.requests.length} requests`);
});
