// **T-982: /surface.html opened while / is already live can stick at "0 tiles · N pending" forever.**
//
// FOUND 2026-09-25 by the explorer on a live server, 2 min into a run (journal window 2): both
// panes read "0 tiles · N pending" (24-40 pending seen), and never resolved. Reproduced here against
// the mock SDR (CLAUDE.md: e2e drives the device interface, receive-only, never the real radio) by
// opening the app at `/` first — following the live edge, exactly as the explorer's session was —
// and only then opening `/surface.html`, the historical preview: the frozen preview's ONE coverage
// survey (`preview.ts`'s `maybeSurvey`, T-580) can answer `complete: false` even though its window
// genuinely reaches back to when recording began, because `t0` and `horizon.recording_began_s`
// arrive independently down two different float64 paths (the client's own carries capture time as
// NANOSECONDS since epoch, ~1.79e18 — past `Number.MAX_SAFE_INTEGER` by ~200x) and can differ by a
// couple hundred nanoseconds despite describing the same instant. A historical surface's bounds
// never move, so the SAME false verdict repeats on every retry, forever: `setSurvey` is never
// called, and T-580's "coverage first" gate then refuses every tile — this file's whole claim.
//
// `ui/test/surface-survey.test.ts` proves the float-precision defect in `decodeSurvey` directly, at
// the unit level, with the exact drift measured here (red on the pre-fix epsilon, green after — see
// its own T-982 case). This is the same defect proven end to end, in a real browser, against a real
// `hk serve` and a real second-tab race for GET /api/coverage's route, which is what the epsilon
// alone cannot prove: that a contended live server actually produces the ~200 ns drift, and that a
// historical page really does read this as "0 tiles" forever rather than "still loading".
import test from "node:test";
import assert from "node:assert/strict";
import { startBackend } from "./backend.mjs";
import { Browser } from "./harness.mjs";

const PORT = Number(process.env.HK_E2E_PORT ?? 8791);

/** The pane's own stated tile count, from its chrome line ("N tiles · M coarse stand-ins · K
 * pending ..."). Reads the FIRST (non-map) pane, which is where the explorer's screenshot showed
 * the stuck state. */
async function paneTileCount(page) {
  const text = await page.eval(`document.querySelector('[data-slot="chrome"]')?.textContent ?? ""`);
  const m = /(\d+) tiles?\s*·/.exec(text);
  return { text, tiles: m ? Number(m[1]) : null };
}

test("T-982: opening /surface.html while / is already live still resolves real tiles, not stuck pending forever", { timeout: 120000 }, async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());

  const browser = await Browser.open();
  t.after(() => browser.close());

  // The app at / — following the live edge, exactly as the explorer's own session had open —
  // running long enough to have issued several rounds of its own GET /api/coverage / GET /api/tiles
  // before the second tab ever asks for anything (T-630's fair-share contention is real only once
  // there is a busy first client to contend with).
  const live = await browser.page(undefined, { newWindow: true });
  assert.equal(await live.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await live.waitForSurfaceMounted();
  await new Promise((r) => setTimeout(r, 8000));

  // NOW open the historical preview, like the explorer did minutes into a run.
  const page = await browser.page(undefined, { newWindow: true });
  assert.equal(await page.goto(`${backend.origin}/surface.html#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted();

  // Poll the pane's own stated tile count. The defect is a PERMANENT freeze, not slow loading, so a
  // generous budget that would tolerate ordinary contention still fails if this never moves off
  // zero — and succeeds the moment it does, however many polls that takes.
  let last = { text: "", tiles: null };
  const t0 = Date.now();
  for (let i = 0; i < 200; i++) {
    last = await paneTileCount(page);
    if (last.tiles !== null && last.tiles > 0) break;
    await new Promise((r) => setTimeout(r, 500));
  }
  assert.ok(last.tiles !== null && last.tiles > 0,
    `the pane never resolved a single tile in ${Date.now() - t0} ms — stuck reading: ${last.text}`);
  t.diagnostic(`resolved ${last.tiles} tile(s) after ${Date.now() - t0} ms`);
});
