// **T-999: the Explore drawer's past-survey "Go" actually moves the pane's TIME, in the real app.**
//
// The unit tier (`ui/test/app-explore-drawer.test.ts`) already proves the pure wiring: `gotoItem`
// folds a row's time window into the same `nav` request as its frequency, `gotoTimeWindow` reads it
// back out (as the window's MIDPOINT, not its end — the review fix), and `centre/surface.ts`
// applies it to the active pane via the real `PaneModel`, with exact expected values. What that
// tier CANNOT see is the defect this ticket was actually filed against: `gotoItem` used to write
// the store's `time` field directly (`reviewAt`), and `centre/surface.ts`'s own per-frame
// `mirror()` — which republishes `s.time` FROM the active pane every render — ran on the very next
// frame and overwrote it with the pane's OLD (unmoved) time before a user ever saw the jump. Only a
// real render loop, over real frames, can demonstrate that the jump survives past the first paint.
//
// So this file: starts a real backend with the mock SDR sweeping (`--device mock:…`, the one legal
// way this tier drives a "device" — CLAUDE.md), waits for the sweep to leave at least one band
// behind (a genuine PAST survey window, not a synthesized one), presses the row's "Go" button in
// the real DOM, and reads the pane's post-click state back from the browser with no debug hook:
// (a) the Live/frozen FAB (`map-controls.ts`'s `.map-fab`), which reads the pane's own `time.live`
// directly — the one thing the pre-fix bug got wrong (the pane never froze, so the FAB stayed lit
// "following"); and (b) the periodic `GET /api/annotations` poll, which every render frame keeps
// pointed at the union of the panes' own (rendered) boxes (`centre/surface.ts`, T-820) — checked
// across two polls a few seconds apart to prove the frozen window HOLDS STILL rather than sliding
// back toward "live" (a following pane would advance every poll; a frozen one does not).
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** `GET` against the backend, retrying `/api/*`'s `503` backpressure (T-454), same shape as the
 * other mock-sweep specs (`scan-everything.e2e.mjs`). */
async function get(backend, urlPath, { tries = 40, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${urlPath}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    if (r.status !== 503 || i >= tries) assert.fail(`GET ${urlPath} -> ${r.status}`);
    await new Promise((res) => setTimeout(res, waitMs));
  }
}

let opening = null;
const opened = () => (opening ??= open());
after(async () => {
  const j = await opening?.catch(() => null);
  j?.browser?.close();
  j?.backend?.stop();
});

async function open() {
  // Its own port (lane base + 32 — every other offset in this range is already taken by another
  // mock-sweep spec: see the comment in scan-everything.e2e.mjs about 8807's 2026-09-23 collision).
  const backend = await startBackend({ port: Number(process.env.HK_E2E_PORT ?? 8791) + 32, mockDevice: true });
  const browser = await Browser.open();
  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load",
    "the app page never fired load");
  await page.waitFor("the app shell to mount its surface slot", `!!document.querySelector('.sf-canvas')`,
    { timeoutMs: 20000 });
  await page.waitForSurfaceMounted({ timeoutMs: 60000 });
  return { page, browser, backend };
}

/** Start a NARROW, fine, fast sweep — over the band the app itself opened on (the fixture's own
 * observed region, `bootstrap.ts`'s opening extent), not `scan-everything.e2e.mjs`'s whole-range
 * one-click. The whole-range sweep's hops never fall inside the Explore drawer's (view-scoped)
 * window in any useful time, since the pane keeps looking where it opened while the DEVICE sweeps
 * elsewhere (CLAUDE.md: a pane's view moves only by an explicit act, never by device tuning it did
 * not ask for) — so a whole-range sweep would need to sweep back around to this exact band before a
 * single past-survey row could appear here. A narrow sweep across the SAME band the pane is looking
 * at produces several closed hops, all inside the drawer's window, in well under its poll interval. */
async function startSweep(page, loMHz, hiMHz) {
  await page.click(`document.querySelector('#review-btn')`);
  await page.waitFor("the review drawer's tab bar to render", `document.querySelectorAll('.rv-tab').length > 0`,
    { timeoutMs: 10000 });
  await page.click(`[...document.querySelectorAll('.rv-tab')].find(b => b.textContent.trim() === 'Device')`);
  await page.waitFor("the Survey-sweep fieldset to mount",
    `[...document.querySelectorAll('.rv-fieldset legend')].some(l => l.textContent.trim() === 'Survey sweep')`,
    { timeoutMs: 10000 });
  const setInput = (placeholder, value) => page.eval(`(() => {
    const e = document.querySelector('input[placeholder="${placeholder}"]');
    e.value = ${JSON.stringify(String(value))};
    e.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  await setInput("from MHz", loMHz);
  await setInput("to MHz", hiMHz);
  await setInput("dwell s", 0.3);
  await page.eval(`(() => {
    const sel = [...document.querySelectorAll('select')].find(s => [...s.options].some(o => o.value === 'fine'));
    sel.value = 'fine';
    sel.dispatchEvent(new Event('change', { bubbles: true }));
  })()`);
  const marker = page.requests.length;
  await page.click(`[...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Start sweep')`);
  const t0 = Date.now();
  for (;;) {
    if (page.requests.slice(marker).some((r) => r.method === "POST" && r.url.includes("/api/control/scan") && r.status === 200)) break;
    if (Date.now() - t0 > 10000) throw new Error("the sweep start never reached POST /api/control/scan");
    await new Promise((res) => setTimeout(res, 100));
  }
  await page.click(`document.querySelector('#review-btn')`); // close the review drawer: it covers the sheet
}

/** Wait until the sweep has visited enough hops to have left at least one behind. */
async function waitForVisitedHops(backend, minSteps, timeoutMs) {
  const t0 = Date.now();
  for (;;) {
    const s = await get(backend, "/api/control/scan").catch(() => null);
    const steps = s?.scan?.progress?.steps_done ?? 0;
    if (steps >= minSteps) return steps;
    if (Date.now() - t0 > timeoutMs) assert.fail(`the sweep only reached ${steps} steps in ${timeoutMs} ms`);
    await new Promise((r) => setTimeout(r, 300));
  }
}

/** A drawer row with a past-survey tag and a real (non-note) "Go" button. */
async function findSurveyRow(page, timeoutMs) {
  await page.waitFor("a past-survey row in the Explore drawer",
    `[...document.querySelectorAll('.explore-drawer li')].some(li =>
       /^survey · (observed|swept) then$/.test(li.querySelector('.tag')?.textContent ?? '') &&
       !!li.querySelector('button.go'))`,
    { timeoutMs });
  return JSON.parse(await page.eval(`JSON.stringify((() => {
    const li = [...document.querySelectorAll('.explore-drawer li')].find(li =>
      /^survey · (observed|swept) then$/.test(li.querySelector('.tag')?.textContent ?? '') &&
      !!li.querySelector('button.go'));
    const r = li.querySelector('button.go').getBoundingClientRect();
    return { title: li.querySelector('.f')?.textContent ?? '', tag: li.querySelector('.tag')?.textContent ?? '',
      goTitle: li.querySelector('button.go').title,
      x: r.x + r.width / 2, y: r.y + r.height / 2 };
  })())`));
}

/** Where the app itself opened (`surface/bootstrap.ts`'s observed-extent rule, T-376/T-441): the
 * band this fixture actually has IQ for, read from the server's own coverage map rather than
 * assumed from the fixture's filename. This is the band the sweep below is scoped to. */
async function openedBandMHz(backend) {
  const q = new URLSearchParams({ f_lo: "1000000", f_hi: "6000000000", cells: "1024", rows: "1" });
  const cov = await get(backend, `/api/coverage?${q}`);
  const cells = cov?.any?.cells ?? [];
  const observed = cells.map((c, i) => (c?.state === "observed" ? i : -1)).filter((i) => i >= 0);
  assert.ok(observed.length > 0, "the fixture's own coverage map shows nothing observed");
  const loHz = 1e6 + (observed[0] / cells.length) * (6e9 - 1e6);
  const hiHz = 1e6 + ((observed[observed.length - 1] + 1) / cells.length) * (6e9 - 1e6);
  return { loMHz: loHz / 1e6 - 1, hiMHz: hiHz / 1e6 + 1 };
}

test("pressing 'Go' on a past-survey row moves the pane to that window, and it sticks past the next frame", async (t) => {
  const { page, backend } = await opened();

  const { loMHz, hiMHz } = await openedBandMHz(backend);
  t.diagnostic(`sweeping the fixture's own observed band: ${loMHz}-${hiMHz} MHz`);
  await startSweep(page, loMHz, hiMHz);
  const steps = await waitForVisitedHops(backend, 3, 60000);
  t.diagnostic(`sweep at ${steps} steps done`);

  // The drawer sheet opens "peek" (a title strip only, docs/23 §10.3): a row's "Go" button is
  // off-screen (clipped by the collapsed sheet's height, not merely scrolled) until it is expanded.
  // The grab handle cycles peek -> half -> full on click (`sheet.ts`); click until it reports full
  // rather than assuming two presses land, since a synthesized click can miss a state.
  for (let i = 0; i < 4 && (await page.eval(`document.querySelector('.sheet')?.dataset.snap`)) !== "full"; i++) {
    await page.click(`document.querySelector('.sheet-grab')`);
    await new Promise((r) => setTimeout(r, 400));
  }
  await page.waitFor("the sheet to reach 'full'", `document.querySelector('.sheet')?.dataset.snap === 'full'`,
    { timeoutMs: 5000 });

  // The window this ticket is about to send the pane to, read from the SAME server record the
  // drawer itself reads — never re-derived from what the client happened to parse. `docs/api.md`:
  // `GET /api/observations?f_lo&f_hi&t0&t1[...]` — the whole tunable range and elapsed wall time.
  const obsQ = new URLSearchParams({ f_lo: "1000000", f_hi: "6000000000", t0: "0", t1: "1e12", limit: "200" });
  const obsBefore = await get(backend, `/api/observations?${obsQ}`);
  t.diagnostic(`server observation records so far: ${obsBefore?.records?.length ?? 0}`);

  const row = await findSurveyRow(page, 30000);
  t.diagnostic(`row: ${JSON.stringify(row)}`);

  // THE load-bearing discriminator: the Live/frozen FAB (`map-controls.ts`'s `.map-fab`, ADR-0013
  // §3.1's "each pane's own Live button" — the exact control this whole ticket's parent SET
  // ACCEPTANCE names). It is driven by `host.isFollowing()` -> `preview.view.panes.isFollowing`,
  // which reads the pane's OWN `time.live` — so unlike a network-timing heuristic, this is a direct
  // window onto "did the pane actually freeze", the one thing the pre-fix bug got wrong: `gotoItem`
  // wrote the STORE's `time` field (`reviewAt`) but never moved the pane, so the pane stayed
  // following and the FAB stayed lit "following" — a jump the user could not tell had failed short
  // of watching the canvas not move. Sweeping keeps the active pane's box addressed at whichever
  // band is currently tuned (`renderFollow`), so before the press the pane is following live.
  await page.waitFor("the map FAB to show 'following' before the Go press",
    `document.querySelector('.map-fab')?.classList.contains('following') === true`, { timeoutMs: 15000 });

  // A CSS expression re-evaluated at press time (`page.click`), not the coordinates captured a
  // moment ago: the drawer re-renders on its own poll and on every scope change, so a coordinate
  // captured earlier can land on nothing (or on whatever replaced it) once that DOM was rebuilt.
  const rowExpr = `[...document.querySelectorAll('.explore-drawer li')].find(li =>
    /^survey · (observed|swept) then$/.test(li.querySelector('.tag')?.textContent ?? '') &&
    !!li.querySelector('button.go'))?.querySelector('button.go')`;
  await page.click(rowExpr);

  await page.waitFor("the map FAB to switch to 'frozen' after Go on a past survey",
    `document.querySelector('.map-fab')?.classList.contains('frozen') === true`, { timeoutMs: 10000 });
  const fabAfter = JSON.parse(await page.eval(`JSON.stringify({
    frozen: document.querySelector('.map-fab').classList.contains('frozen'),
    following: document.querySelector('.map-fab').classList.contains('following'),
    ariaPressed: document.querySelector('.map-fab').getAttribute('aria-pressed'),
    title: document.querySelector('.map-fab').title,
  })`));
  t.diagnostic(`FAB after Go: ${JSON.stringify(fabAfter)}`);
  assert.equal(fabAfter.frozen, true, "the pane never froze on the survey's window — the jump was lost");
  assert.equal(fabAfter.following, false);
  assert.equal(fabAfter.ariaPressed, "false");

  // Corroborating: the periodic `/api/annotations` poll (T-820) — which every render frame keeps
  // addressed at the union of the panes' own (rendered) boxes — should now be asking about a window
  // well behind the live edge, and hold still there rather than sliding back toward it.
  const annotationsAfter = async (sinceIdx, timeoutMs) => {
    const t0 = Date.now();
    for (;;) {
      const hit = page.requests.slice(sinceIdx).find((r) => r.url.includes("/api/annotations") && r.status === 200);
      if (hit) return new URL(hit.url);
      if (Date.now() - t0 > timeoutMs) return null;
      await new Promise((res) => setTimeout(res, 200));
    }
  };
  const marker = page.requests.length;
  const first = await annotationsAfter(marker, 6000);
  assert.ok(first, "no /api/annotations poll fired once the pane froze");
  const t0First = Number(first.searchParams.get("t0")), t1First = Number(first.searchParams.get("t1"));
  await new Promise((r) => setTimeout(r, 4000));
  const timeline = await get(backend, "/api/timeline").catch(() => null);
  const edgeNowS = timeline?.window?.t1_s ?? null;
  const second = await annotationsAfter(marker, 6000);
  assert.ok(second, "no later /api/annotations poll to compare against");
  const t0Second = Number(second.searchParams.get("t0")), t1Second = Number(second.searchParams.get("t1"));
  t.diagnostic(`annotations window: first ${t0First}..${t1First}, later ${t0Second}..${t1Second} (live edge ${edgeNowS})`);
  assert.ok(Math.abs(t0Second - t0First) < 5 && Math.abs(t1Second - t1First) < 5,
    `the pane's time window drifted after freezing (${t0First}..${t1First} -> ${t0Second}..${t1Second})`);
  if (edgeNowS !== null) assert.ok(edgeNowS - t1Second > 1, `the frozen window (ending ${t1Second}) is not behind the live edge (${edgeNowS})`);

  await page.shot(path.join(ART, "drawer-goto-after.png"));
  assert.deepEqual(page.exceptions, [], "uncaught exception");
});
