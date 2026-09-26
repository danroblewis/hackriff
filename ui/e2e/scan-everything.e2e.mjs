// **T-516: the survey sweep is reachable, and the one-click "scan everything" visibly fills the
// canvas live.** The scan engine is already built (T-406/T-452/T-439/T-457/T-460); this file does
// not exercise the engine's own arithmetic (that is `ui/test/scan-control.test.ts` and
// `crates/hk-cli/tests/api_contract.rs`). It exercises the one thing no unit tier can: that a real
// user, in the real shipped app, can find the control and that pressing it makes the canvas fill
// while it runs — over the MOCK SDR (CLAUDE.md: e2e drives the system through the device interface,
// never files straight into the pipeline). A `--replay` backend reports no tunable range, so the
// sweep control would be correctly stated-and-disabled there and prove nothing about a live front
// end; this brings up its own `hk serve --device mock:…`, the way canvas-journey.e2e.mjs does.
//
// WHAT EACH ASSERTION IS A PROPERTY OF:
//   1. REACHABLE  — the DOM the device panel actually renders, found by the same click path a user
//                   takes, never by source inspection (that already exists in scan-control.test.ts's
//                   "mounted" tests). T-1007 moved that path: the device controls are SETTINGS now,
//                   so it is the map's ⋯ menu -> "Device & display…", not Review -> "Device". Review
//                   keeps only anomalies/alarms, and this spec asks for the panel where it now lives.
//   2. FILLS LIVE — the server's OWN `/api/coverage` answer over the full 1 MHz-6 GHz range, and
//                   the sweep's own `/api/control/scan` progress counter, before and after the
//                   sweep has been running a while — never inferred from a single screenshot. The
//                   pane, zoomed out to the whole 1 MHz-6 GHz range with the cluster's own Zoom-out
//                   (the minimap that used to show it with no navigation is retired, T-995), is read
//                   too, as corroborating pixel evidence, reported alongside the server's own numbers.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** Zoom-out presses that take a pane from the opening window to the whole device range (at 1/0.6
 * a press, ~16 do it from 2.4 MHz; the rest are margin — a press at the bound changes nothing). */
const ZOOM_OUT_PRESSES = 30;

/** `GET` against the backend, retrying `/api/*`'s `503` backpressure rather than reading it as a
 * refusal (T-454) — the same shape as `canvas-journey.e2e.mjs`'s `get`. */
async function get(backend, urlPath, { tries = 40, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${urlPath}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    if (r.status !== 503 || i >= tries) assert.fail(`GET ${urlPath} -> ${r.status}`);
    await new Promise((res) => setTimeout(res, waitMs));
  }
}

/** Cells the server states `"observed"` over the FULL tunable range — the load-bearing number for
 * "a sweep fills the canvas", read from the coverage map itself rather than from what a screenshot
 * happens to show. `unknown` is excluded from the denominator (T-423), same reasoning as
 * `canvas-journey.e2e.mjs`'s `coverage()`. */
async function observedOverWholeRange(backend, cells = 512) {
  const q = new URLSearchParams({ f_lo: "1000000", f_hi: "6000000000", cells: String(cells), rows: "8" });
  const cov = await get(backend, `/api/coverage?${q}`);
  const cs = cov?.any?.cells ?? [];
  return cs.filter((c) => c?.state === "observed").length;
}

async function waitForCoverage(backend, timeoutMs) {
  const t0 = Date.now();
  for (;;) {
    const n = await observedOverWholeRange(backend, 128).catch(() => 0);
    if (n > 0) return n;
    if (Date.now() - t0 > timeoutMs) throw new Error(`the mock SDR put nothing in the coverage map in ${timeoutMs} ms`);
    await new Promise((r) => setTimeout(r, 500));
  }
}

// ---------------------------------------------------------------------------
// The one backend, the one browser, the one page
// ---------------------------------------------------------------------------

let opening = null;
const opened = () => (opening ??= open());
after(async () => {
  const j = await opening?.catch(() => null);
  j?.browser?.close();
  j?.backend?.stop();
});

async function open() {
  // Its own port (never 8789/8899/8900 — the user's live-HackRF demo) and its own mock backend;
  // the shared HK_E2E_PORT backend other files use is a plain --replay with no tunable range.
  // The port is the LANE's base + 12, not a constant: the constant 8807 was shared with
  // fog-of-war.e2e.mjs, and at three lanes the two ran side by side on 2026-09-23 - one of them
  // was stepped past the busy port and the other's spec then talked to the wrong server.
  const backend = await startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 12, mockDevice: true });
  await waitForCoverage(backend, 60000);
  const browser = await Browser.open();
  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load",
    "the app page never fired load");
  await page.waitFor("the app shell to mount its surface slot", `!!document.querySelector('.sf-canvas')`,
    { timeoutMs: 20000 });
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  return { page, browser, backend };
}

// ---------------------------------------------------------------------------
// 1. Reachable
// ---------------------------------------------------------------------------

test("the Survey-sweep panel, and the one-click 'scan everything' beside it, are reachable from the app a new user opens", async () => {
  const { page } = await opened();

  await page.click(`document.querySelector('.map-more-btn')`);
  await page.waitFor("the settings menu to open", `!document.querySelector('#map-more-menu').hidden`, { timeoutMs: 10000 });
  await page.click(`document.querySelector('#map-more-menu [data-panel="device"]')`);
  await page.waitFor("the Survey-sweep fieldset to mount",
    `[...document.querySelectorAll('.rv-fieldset legend')].some(l => l.textContent.trim() === 'Survey sweep')`,
    { timeoutMs: 10000 });

  const buttonText = (label) =>
    `[...document.querySelectorAll('button')].find(b => b.textContent.trim() === ${JSON.stringify(label)})?.textContent ?? null`;
  assert.ok(await page.eval(buttonText("Scan everything (fast)")),
    "the one-click is not on the page a user actually opens (only in source, which scan-control.test.ts already checks)");
  assert.ok(await page.eval(buttonText("Start sweep")), "the plain Start sweep control is not on the page");

  // The From/To/Dwell/Step controls the one-click fills in for the user.
  assert.equal(await page.$count('input[placeholder="from MHz"]'), 1);
  assert.equal(await page.$count('input[placeholder="to MHz"]'), 1);
  assert.equal(await page.$count('input[placeholder="dwell s"]'), 1);
  const stepOptions = await page.eval(
    `[...document.querySelectorAll('select option')].filter(o => o.value === 'fine' || o.value === 'coarse').map(o => o.value)`);
  assert.deepEqual([...stepOptions].sort(), ["coarse", "fine"], "T-517's step control is not beside the sweep");

  // Reachable also means USABLE: a control nobody can press is not a control (the T-409 rule) — on
  // a live mock front end the one-click must not be silently disabled.
  const scanAllDisabled = await page.eval(
    `[...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Scan everything (fast)')?.disabled`);
  const panelText = (await page.eval(`document.querySelector('.rv-tabpanel:not([hidden])')?.textContent ?? ''`)) ?? "";
  assert.equal(scanAllDisabled, false, `the one-click is disabled on a live mock front end: ${panelText.slice(0, 500)}`);
});

// ---------------------------------------------------------------------------
// 2. Fills live
// ---------------------------------------------------------------------------

test("pressing 'scan everything' starts a coarse, fast full-range sweep that visibly fills the canvas in real time", async (t) => {
  const { page, backend } = await opened();

  const before = await observedOverWholeRange(backend, 512);
  const canvasRect = await page.$rect(".sf-canvas");
  assert.ok(canvasRect && canvasRect.w > 200 && canvasRect.h > 100, `no usable canvas box: ${JSON.stringify(canvasRect)}`);
  // The whole range is reached by zooming the pane out (T-995 retired the minimap that showed it).
  // Pressed through the element (the review drawer test 1 opened may sit over the cluster).
  await page.eval(`(() => { for (let i = 0; i < ${ZOOM_OUT_PRESSES}; i++) document.querySelector('.map-zoom-out')?.click(); })()`);
  await page.frames(4);
  const ins = await page.canvasInsets();
  const paneRect = {
    x: Math.round(canvasRect.x), w: Math.round(canvasRect.w),
    y: Math.round(canvasRect.y + ins.top), h: Math.round(canvasRect.h - ins.top - ins.bottom),
  };
  const beforeShot = await page.shot(path.join(ART, "scan-everything-zoomed-out-before.png"));
  const beforeCensus = census(beforeShot, paneRect);

  // The press itself: the same click path test 1 just proved reaches a real, enabled button.
  await page.click(`[...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Scan everything (fast)')`);

  // The request actually reached the route docs/api.md names (T-367's lesson: assert the request
  // the client builds, on the wire — never only that the panel renders something plausible).
  const sawStart = async (timeoutMs) => {
    const t0 = Date.now();
    for (;;) {
      if (page.requests.some((r) => r.method === "POST" && r.url.includes("/api/control/scan") && r.status === 200)) return true;
      if (Date.now() - t0 > timeoutMs) return false;
      await new Promise((res) => setTimeout(res, 100));
    }
  };
  assert.ok(await sawStart(10000), "the one-click never reached POST /api/control/scan");

  // The server's own account of what started: COARSE, at the FAST dwell, priced before it ran.
  const started = await get(backend, "/api/control/scan");
  assert.equal(started?.scan?.state, "running", `the sweep did not start running: ${JSON.stringify(started?.scan)}`);
  const plan = started.scan.plan;
  assert.equal(plan.step, "coarse", `the one-click did not start a COARSE sweep: ${JSON.stringify(plan)}`);
  assert.ok(plan.dwell_s >= 0.2 && plan.dwell_s <= 0.5, `expected the fast 0.2-0.5 s dwell, got ${plan.dwell_s}`);
  t.diagnostic(`coarse full-range plan: ${plan.steps} steps of ${(started.scan.budget.step_span_hz / 1e6).toFixed(1)} MHz` +
    ` per hop, priced pass ${started.scan.budget.pass_s.toFixed(1)} s (${(started.scan.budget.pass_s / 60).toFixed(1)} min)`);

  // Watch it actually step and the coverage map actually widen — polled over a real window, not a
  // single screenshot (T-473's lesson about single-moment evidence applies here too).
  const WINDOW_MS = 30000, POLL_MS = 2000;
  const t0 = Date.now();
  const series = [];
  const firstProgress = started.scan.progress?.steps_done ?? 0;
  while (Date.now() - t0 < WINDOW_MS) {
    const s = await get(backend, "/api/control/scan");
    const obs = await observedOverWholeRange(backend, 512);
    series.push({ atMs: Date.now() - t0, state: s.scan.state, stepsDone: s.scan.progress?.steps_done ?? 0, observed: obs });
    await new Promise((r) => setTimeout(r, POLL_MS));
  }
  const last = series[series.length - 1];
  t.diagnostic(`observed/512 over 1 MHz-6 GHz: before ${before} -> after ${WINDOW_MS / 1000}s ${last.observed} ` +
    `(steps_done ${firstProgress} -> ${last.stepsDone}); series: ${JSON.stringify(series.map((s) => [s.atMs, s.stepsDone, s.observed]))}`);

  assert.ok(last.stepsDone > firstProgress,
    `the sweep never advanced a step in ${WINDOW_MS} ms: stuck at ${firstProgress} (${JSON.stringify(series)})`);
  assert.ok(last.observed > before,
    `the coverage map did not visibly grow in ${WINDOW_MS} ms: before ${before}/512, after ${last.observed}/512 ` +
    `(${JSON.stringify(series)})`);

  // Corroborating pixel evidence from the pane, zoomed out to the whole 1 MHz-6 GHz range. Reported with numbers either way: a widening
  // server-side coverage map that produces no new pixels anywhere on screen would itself be a
  // finding (data present, never rendered — the "we have it but didn't render it" bug CLAUDE.md
  // names), so this is read and reported, not silently skipped once the sharp assertions above hold.
  await page.frames(4);
  const afterShot = await page.shot(path.join(ART, "scan-everything-zoomed-out-after.png"));
  const afterCensus = census(afterShot, paneRect);
  t.diagnostic(`zoomed-out pane census before: ${beforeCensus.distinct} distinct, dominant ${beforeCensus.dominant} ` +
    `at ${(beforeCensus.dominantShare * 100).toFixed(1)} %; after: ${afterCensus.distinct} distinct, dominant ` +
    `${afterCensus.dominant} at ${(afterCensus.dominantShare * 100).toFixed(1)} %`);
});
