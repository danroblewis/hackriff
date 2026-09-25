// **T-1008: Scan as a buttoned map overlay** — over the MOCK SDR (CLAUDE.md: e2e drives the system
// through the device interface; a `--replay` backend has no tunable range, so the Scan button would
// be correctly disabled there and prove nothing), in the real shipped app.
//
// WHAT EACH ASSERTION IS A PROPERTY OF — the ticket's acceptance, one test each:
//
//   1. THE PLAN DRAWN IS THE PLAN PRICED, AND GREY INSIDE IT IS VISIBLY PART OF IT. The small Scan
//      button in the Go-to cluster opens a plan over the active pane; the steps the overlay draws
//      (the panel's own statement of what `scanPlanQuads` is handed) equal the server's own price
//      of that range (`GET /api/control/scan?…&windows=1`, fetched here independently). Then
//      PIXELS: columns of the pane that are grey (unobserved — checked against the server's own
//      `/api/coverage`, not a colour guess) and inside the plan carry the plan's hatch after the
//      button, and carried none of it before. Nothing reached a device route while planning.
//   2. DRAGGING AN EDGE RE-PLANS. A real pointer drag on the plan's right edge narrows the region,
//      the server re-prices it, and the drawn steps are the new price — again equal to an
//      independent `GET`.
//   3. THE DRAWN PLAN IS THE EXECUTED PLAN. Start (a real click), let the engine take steps, and
//      compare the overlay's windows to the scheduler's own log: every per-step dwell record in
//      `/api/observations` since Start is centred on a drawn window's centre, in the drawn order.
//      The overlay shows progress (a dwelling step) while it runs.
//   4. STOP HALTS THE SCAN, FROM THE SAME BUTTON. The button now reads Stop; pressing it idles the
//      sweep (`GET /api/control/scan`), the radio stops stepping (its centre holds over several
//      dwells), and the overlay is gone.
//   Screenshots at 1280×800 and 400 px wide go to the artifacts directory.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");
/** The mock tunes to the tuning step, not to the exact hop centre (HackRF: 30 MHz / 2^20). */
const TUNE_STEP_HZ = 30e6 / 2 ** 20;
const DWELL_S = 1;
/** How far behind the wall clock the mock's capture clock may run (measured ~1 s). */
const CAPTURE_LAG_S = 4;

async function api(backend, urlPath, { method = "GET", body, tries = 40 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${urlPath}`, {
      method, headers: { authorization: `Bearer ${backend.token}`, ...(body ? { "content-type": "application/json" } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    });
    if (r.ok) return r.json();
    if (r.status !== 503 || i >= tries) assert.fail(`${method} ${urlPath} -> ${r.status} ${await r.text()}`);
    await new Promise((res) => setTimeout(res, 200));
  }
}

let opening = null;
const opened = () => (opening ??= open());
after(async () => {
  const j = await opening?.catch(() => null);
  if (j?.backend) await api(j.backend, "/api/control/scan/stop", { method: "POST", body: {} }).catch(() => {});
  j?.browser?.close();
  j?.backend?.stop();
});

async function mountApp(browser, backend, width, height) {
  const page = await browser.page(undefined, { width, height, newWindow: width !== 1280 });
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load", "the app page never fired load");
  await page.waitFor("the app shell to mount its surface slot", `!!document.querySelector('.sf-canvas')`, { timeoutMs: 20000 });
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  await page.waitFor("the Scan button to be enabled by the control-state poll",
    `(() => { const b = document.querySelector('.map-scan-btn'); return !!b && !b.disabled; })()`, { timeoutMs: 20000 });
  return page;
}

async function open() {
  // Its own mock backend on its own port (the lane's base + 4; never the user's demo ports).
  const backend = await startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 4, mockDevice: true });
  const browser = await Browser.open();
  const page = await mountApp(browser, backend, 1280, 800);
  return { page, browser, backend };
}

/** What the overlay draws, as the panel states it (the model `scanPlanQuads` is handed). */
async function drawn(page) {
  return page.eval(`(() => { const p = document.querySelector('.map-scan');
    const d = p?.dataset ?? {};
    return { plan: d.plan ? JSON.parse(d.plan) : null, hidden: !!p?.hidden,
      pane: d.paneF0Hz ? { f0: +d.paneF0Hz, f1: +d.paneF1Hz, left: +d.paneLeftPx, w: +d.paneWPx, top: +d.paneTopPx, h: +d.paneHPx } : null }; })()`);
}

/** The server's own price of a range, with its steps, as `[lo, hi, centre]` like the panel states. */
async function serverSteps(backend, lo, hi) {
  const q = new URLSearchParams({ f_lo_hz: String(lo), f_hi_hz: String(hi), dwell_s: String(DWELL_S), step: "fine", windows: "1" });
  const a = await api(backend, `/api/control/scan?${q}`);
  return a.proposed.plan.windows.map((w) => [w.lo_hz, w.hi_hz, w.center_hz]);
}

/** Lime hatch pixels (`SCAN_INK` 0.62/1.0/0.22, blended) in a vertical stripe of the screenshot. */
function limeIn(img, x0, x1, y0, y1) {
  let n = 0, total = 0;
  for (let y = Math.max(0, Math.round(y0)); y < Math.min(img.height, Math.round(y1)); y++) {
    for (let x = Math.max(0, Math.round(x0)); x < Math.min(img.width, Math.round(x1)); x++) {
      const i = (y * img.width + x) * 4;
      const [r, g, b] = [img.data[i], img.data[i + 1], img.data[i + 2]];
      total++;
      // Lime over grey: green well above blue and red. Grey (r = g = b), the white/grey chrome
      // text and the cyan-to-yellow ramp stops never have green this far above BOTH.
      if (g > 90 && g - b > 45 && g - r > 25) n++;
    }
  }
  return { n, total };
}

let canvasRect = null;
const cssX = (pane, f) => canvasRect.x + pane.left + ((f - pane.f0) / (pane.f1 - pane.f0)) * pane.w;

// ---------------------------------------------------------------------------
// 1. The plan drawn is the plan priced; grey inside it is visibly part of it
// ---------------------------------------------------------------------------

let firstPlan = null;
test("Scan opens a plan over the view: the steps drawn are the server's price, hatched over grey too", async () => {
  const { page, backend } = await opened();
  canvasRect = await page.$rect(".sf-canvas");
  // Widen the view past the tuned window so the plan spans unobserved (grey) spectrum on both sides.
  for (let i = 0; i < 3; i++) { await page.click("document.querySelector('.map-zoom-out')"); await page.frames(3); }
  await page.frames(10);
  const posts0 = page.requests.filter((r) => r.method === "POST").length;
  const before = await page.shot();

  await page.click("document.querySelector('.map-scan-btn')");
  await page.waitFor("the plan to be priced with its steps",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; return !!d && !!JSON.parse(d).windows; })()`,
    { timeoutMs: 20000 });
  await page.frames(6);
  const d = await drawn(page);
  firstPlan = d;
  assert.equal(d.plan.state, "plan");
  assert.equal(d.hidden, false, "the plan panel is shown");
  assert.match(String(d.plan.device_id), /^mock:/, "the plan names the device that would do it");
  assert.ok(d.plan.windows.length >= 2, `a widened view is more than one step (${d.plan.windows.length})`);
  assert.deepEqual(d.plan.windows, await serverSteps(backend, d.plan.lo_hz, d.plan.hi_hz),
    "the steps drawn are exactly the steps the server prices for that range — never the client's own tiling");
  const panelText = await page.eval(`document.querySelector('.map-scan').textContent`);
  assert.match(panelText, new RegExp(`${d.plan.windows.length} steps × ${DWELL_S} s`), "the panel states the server's own arithmetic");
  assert.equal(page.requests.filter((r) => r.method === "POST").length, posts0, "opening and pricing a plan posted nothing");

  // PIXELS over grey. Which spectrum is grey is the server's word (`/api/coverage`), not a colour guess.
  const pane = d.pane;
  assert.ok(pane, "the active pane's geometry is stated beside the plan");
  const cells = 64;
  const now = Date.now() / 1000;
  const cov = await api(backend, `/api/coverage?${new URLSearchParams({ f_lo: String(pane.f0), f_hi: String(pane.f1), t0: String(now - 30), t1: String(now), cells: String(cells), rows: "1" })}`);
  const states = (cov.any?.cells ?? []).map((c) => c?.state);
  const lo = d.plan.windows[0][0], hi = d.plan.windows.at(-1)[1];
  const greyInPlan = [];
  for (let i = 0; i < states.length; i++) {
    const f0 = pane.f0 + ((pane.f1 - pane.f0) * i) / cells, f1 = pane.f0 + ((pane.f1 - pane.f0) * (i + 1)) / cells;
    if (states[i] === "unobserved" && f0 > lo && f1 < hi) greyInPlan.push([f0, f1]);
  }
  assert.ok(greyInPlan.length >= 2, `the plan spans cells the server says are unobserved (${greyInPlan.length}; states ${JSON.stringify(states)})`);
  const after = await page.shot(path.join(ART, "scan-overlay-plan-1280x800.png"));
  const y0 = canvasRect.y + pane.top + 0.15 * pane.h, y1 = canvasRect.y + pane.top + 0.85 * pane.h;
  let inkBefore = 0, inkAfter = 0, px = 0;
  for (const [f0, f1] of greyInPlan) {
    const x0 = cssX(pane, f0) + 1, x1 = cssX(pane, f1) - 1;
    inkBefore += limeIn(before, x0, x1, y0, y1).n;
    const a = limeIn(after, x0, x1, y0, y1);
    inkAfter += a.n; px += a.total;
  }
  assert.equal(inkBefore, 0, "no plan ink was on the grey before Scan was pressed");
  assert.ok(inkAfter / px > 0.05, `grey inside the plan carries its hatch (${inkAfter}/${px} px)`);
  assert.ok(inkAfter / px < 0.6, `…as a hatch, not a wash: the grey still shows between the lines (${inkAfter}/${px})`);
});

// ---------------------------------------------------------------------------
// 2. Dragging an edge re-plans
// ---------------------------------------------------------------------------

let planned = null;
test("dragging the plan's right edge narrows it, and the steps drawn are the server's new price", async () => {
  const { page, backend } = await opened();
  assert.ok(firstPlan, "the first test opened a plan");
  const { plan, pane } = await drawn(page);
  // A third of the way down the pane: clear of the zoom stack (mid-height, right edge) and the panel.
  const x = cssX(pane, plan.hi_hz), y = canvasRect.y + pane.top + pane.h * 0.3;
  assert.ok(x < canvasRect.x + pane.left + pane.w - 20, "the plan's right edge is on screen, inside the pane, as a handle");
  const to = cssX(pane, plan.lo_hz + (plan.hi_hz - plan.lo_hz) * 0.55);
  const posts0 = page.requests.filter((r) => r.method === "POST").length;
  await page.drag({ x, y }, { x: to, y }, 10);
  await page.waitFor("the dragged plan to be re-priced",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; if (!d) return false; const p = JSON.parse(d);
      return !!p.windows && p.hi_hz < ${plan.hi_hz - 1e5}; })()`, { timeoutMs: 20000 });
  await page.frames(4);
  const d = await drawn(page);
  assert.ok(Math.abs(d.plan.lo_hz - plan.lo_hz) < 1, "the other edge stayed put");
  assert.ok(d.plan.windows.length < plan.windows.length, `fewer steps (${d.plan.windows.length} < ${plan.windows.length})`);
  assert.deepEqual(d.plan.windows, await serverSteps(backend, d.plan.lo_hz, d.plan.hi_hz),
    "the re-planned steps are the server's price of the new region");
  assert.equal(page.requests.filter((r) => r.method === "POST").length, posts0, "a drag commands nothing");
  planned = d.plan;
});

// ---------------------------------------------------------------------------
// 3. The drawn plan is the executed plan
// ---------------------------------------------------------------------------

test("Start: the steps the engine executes (the scheduler's log) are the steps the overlay drew", async () => {
  const { page, backend } = await opened();
  assert.ok(planned, "a plan was dragged into shape");
  const startS = Date.now() / 1000;
  await page.click("document.querySelector('.map-scan-go')");
  await page.waitFor("the button to become Stop",
    `document.querySelector('.map-scan-btn .map-scan-label')?.textContent === 'Stop'`, { timeoutMs: 15000 });
  const post = page.requests.filter((r) => r.method === "POST" && r.url.endsWith("/api/control/scan"));
  assert.equal(post.length, 1, "exactly one Start");

  const n = Math.min(3, planned.windows.length);
  let s;
  const t0 = Date.now();
  for (;;) {
    s = (await api(backend, "/api/control/scan")).scan;
    if (s.state === "running" && (s.progress?.steps_done ?? 0) >= n + 1) break;
    assert.ok(Date.now() - t0 < 60000, `the sweep never took ${n + 1} steps: ${JSON.stringify(s)}`);
    await new Promise((r) => setTimeout(r, 300));
  }
  // The overlay draws the RUNNING plan with the same steps it drew as a plan, and shows progress.
  await page.waitFor("the overlay to draw the running sweep with a dwelling step",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; if (!d) return false; const p = JSON.parse(d);
      return p.state === 'running' && !!p.windows && p.dwell_step !== null; })()`, { timeoutMs: 15000 });
  const run = (await drawn(page)).plan;
  assert.deepEqual(run.windows, planned.windows, "the sweep runs the plan that was drawn and priced");
  assert.equal(run.editable, false, "a running sweep is not draggable");
  await page.shot(path.join(ART, "scan-overlay-running-1280x800.png"));

  // The scheduler's log: one dwell record per executed step, with its own centre.
  let executed = [];
  let raw = [];
  const t1 = Date.now();
  for (;;) {
    const q = new URLSearchParams({ f_lo: String(planned.lo_hz - 5e6), f_hi: String(planned.hi_hz + 5e6), t0: String(startS), t1: String(Date.now() / 1000 + 1), limit: "1000" });
    const obs = await api(backend, `/api/observations?${q}`);
    raw = (obs.records ?? []).filter((r) => r.record === "dwell").map((r) => ({
      c: r.window?.center_hz, tier: r.tier, reason: r.reason?.code, start: (r.observed?.start_ns ?? 0) / 1e9 - startS,
      end: (r.observed?.end_ns ?? 0) / 1e9 - startS }));
    executed = (obs.records ?? [])
      // Records are in CAPTURE time, which on the mock runs ~1 s behind the wall clock `startS` was
      // read from — so the cut is "began within a few seconds of Start", which excludes only the
      // tune the radio held before Start (it began when the page opened, tens of seconds earlier).
      .filter((r) => r.record === "dwell" && r.window && (r.observed?.start_ns ?? 0) / 1e9 >= startS - CAPTURE_LAG_S)
      .sort((a, b) => a.observed.start_ns - b.observed.start_ns)
      .map((r) => r.window.center_hz);
    if (executed.length >= n) break;
    assert.ok(Date.now() - t1 < 60000, `the scheduler's log never showed ${n} steps: ${JSON.stringify(executed)}`);
    await new Promise((r) => setTimeout(r, 500));
  }
  const drawnCentres = planned.windows.map((w) => w[2]);
  for (const [i, c] of executed.slice(0, n).entries()) {
    assert.ok(Math.abs(c - drawnCentres[i]) <= TUNE_STEP_HZ,
      `executed step ${i} was centred on ${c} Hz; the overlay drew step ${i} at ${drawnCentres[i]} Hz (drawn: ${JSON.stringify(drawnCentres)}; log since Start: ${JSON.stringify(raw)})`);
    const w = planned.windows[i];
    assert.ok(c >= w[0] && c <= w[1], `executed step ${i} lies inside the slice the overlay drew for it`);
  }
});

// ---------------------------------------------------------------------------
// 4. Stop halts the scan, from the same button
// ---------------------------------------------------------------------------

test("Stop, from the same button, halts the sweep: idle, the radio stops stepping, the overlay is gone", async () => {
  const { page, backend } = await opened();
  await page.click("document.querySelector('.map-scan-btn')");
  await page.waitFor("the button to read Scan again",
    `document.querySelector('.map-scan-btn .map-scan-label')?.textContent === 'Scan'`, { timeoutMs: 15000 });
  assert.ok(page.requests.some((r) => r.method === "POST" && r.url.endsWith("/api/control/scan/stop")), "Stop posted the stop route");
  const s = (await api(backend, "/api/control/scan")).scan;
  assert.equal(s.state, "idle", JSON.stringify(s));
  assert.equal(s.progress, null);
  const centre = async () => (await api(backend, "/api/control/state")).tuning?.center_hz;
  const c0 = await centre();
  await new Promise((r) => setTimeout(r, (DWELL_S * 3 + 0.5) * 1000));
  assert.equal(await centre(), c0, "the radio kept stepping after Stop");
  const d = await drawn(page);
  assert.equal(d.plan, null, "the overlay is gone");
  assert.equal(d.hidden, true, "the panel closed");
});

// ---------------------------------------------------------------------------
// The narrow layout: the same button and plan at 400 px
// ---------------------------------------------------------------------------

test("at 400 px wide the Scan button is reachable and its plan draws and prices", async () => {
  const { browser, backend } = await opened();
  const page = await mountApp(browser, backend, 400, 800);
  await page.click("document.querySelector('.map-scan-btn')");
  await page.waitFor("the plan to be priced with its steps at 400 px",
    `(() => { const d = document.querySelector('.map-scan')?.dataset?.plan; return !!d && !!JSON.parse(d).windows; })()`,
    { timeoutMs: 20000 });
  await page.frames(6);
  const r = await page.$rect(".map-scan");
  assert.ok(r.x >= 0 && r.x + r.w <= 400 + 0.5, `the panel fits the narrow screen (${JSON.stringify(r)})`);
  await page.shot(path.join(ART, "scan-overlay-plan-400.png"));
  await page.click("document.querySelector('.map-scan-x')");
  await page.waitFor("the plan to close", `!document.querySelector('.map-scan')?.dataset?.plan`, { timeoutMs: 5000 });
});
