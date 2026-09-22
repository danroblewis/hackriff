// **T-521: the standing guard for ADR-0020's last-known/shadow tier.**
//
// 2A (T-519, backend `shadow` search) and 2B (T-520, `cellrule.ts`'s `CELL.SHADOW` mark) are
// merged. This file is what keeps them honest: drive the MOCK SDR (CLAUDE.md's e2e rule — through
// the device interface, never files into the pipeline) through exactly the three-part claim the
// user asked for, in a real browser —
//
//   1. sweep a band, then move away  -> that band renders SHADOW (dim, scanlined), not grey;
//   2. a band never swept at all     -> stays GREY, always;
//   3. re-sweep the departed band    -> it returns to full brightness.
//
// and fail if a shadow is EVER drawn over spectrum this run's radio never looked at — the one
// failure ADR-0020 exists to make impossible.
//
// ——— THE SCENE, AND WHY THREE SEPARATE BANDS RATHER THAN SUB-DIVIDING ONE RECORDING ———
//
// The fixture (`fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta`) is a real FM capture, 100.8 MHz wide
// by 2.4 MHz, with a genuine signal (pilot + RDS) at 101.3 MHz. The mock's own declared sample-rate
// floor is `min(2 Msps, recording rate)` = **2 Msps** (`crates/hk-core/src/source/mock/mod.rs`,
// `mock_capabilities`), so no retune this device will ever take can be narrower than 2 MHz — and
// the whole recording is only 2.4 MHz wide, so two DIFFERENT 2-MHz-floor windows *inside* it would
// necessarily overlap almost entirely. So band A is the recording's own live signal (101.3 MHz),
// band B is a retune far outside it (433.92 MHz — served as synthesised noise-floor per the mock's
// own doc comment, still a real, measurable capture: "outside coverage... complex white Gaussian
// noise at the recording's estimated floor PSD"), and band C (200 MHz) is never commanded at all.
// The mock's frequency range is the full HackRF one (1 MHz - 6 GHz, `mock_capabilities`), so every
// address below is addressable; only B and A are ever actually TUNED to.
//
// ——— HOW EACH CLAIM IS MEASURED — PIXELS, TIED TO THE SERVER'S OWN PLANES, NOT HARDCODED COLOUR ———
//
// Per the brief: measure in pixels, and check against the server's own `coverage` and `shadow`
// planes where possible, not hardcoded colours. Two techniques do that:
//
//  - **The grey and ink colours are HARVESTED FROM THE PRODUCT'S OWN RENDERING**, not hardcoded.
//    `/surface.html`'s legend (`ui/src/surface/legend.ts`) paints each mark's swatch through
//    `cellPixel` — the exact function the shader is generated from — so reading its canvas's own
//    `getImageData` is reading the product's current definition of "grey" and "the shadow's ink",
//    whatever they are today. T-526 made the shadow's brightness a per-viewer setting
//    (`uShadowGain`, default 0.25, clamp 0.05-0.7): this file never assumes 0.25, and the ink colour
//    is fixed regardless of gain (`SHADOW_MARK.ink`), so harvesting it once at the start survives
//    T-523 raising the default. The grey swatch is a single flat colour by construction
//    (`CELL_MARKS[UNOBSERVED]` has no pattern) — any pixel gives it. The ink is derived as the ONE
//    colour common to three columns sampled at different ramp positions (x = 0, 0.5, 1): a shadow
//    cell alternates a ground that varies with x and a fixed ink that does not, so the ink is
//    exactly their intersection. (A first version assumed the ink was always the minority colour
//    within one column's 22 rows and got it backwards at the ramp's near-white top, where the
//    dimmed ground rounds to a plain grey that happened to be the minority there — see
//    `inkFromSwatch`'s own comment for the measurement that caught it.)
//  - **The `shadow`/`coverage` planes are read from the SAME tile responses the pane itself
//    fetched**, replayed by URL from `page.requests` and re-`fetch`ed with the same bearer token —
//    not addressed by hand-deriving a tile's `(level_f, level_t, f_index, t_index)`, which would be
//    a second implementation of `ui/src/surface/tile.ts`'s own addressing (the T-397 trap this repo
//    keeps naming). The response actually used to draw the frame **is** the ground truth for what
//    was drawn, so replaying its own URL ties the pixels to the server's planes directly.
//
// Brightness is compared RELATIVELY (shadow vs. its own prior live baseline, and vs. its own later
// re-swept baseline) rather than against a computed absolute ceiling, so the gate does not need to
// know the gain in force either.
//
// ——— WHAT IS A HARD GATE AND WHAT IS DIAGNOSTIC ———
//
// The scanline TEXTURE (ink-coloured pixels) is reported but not gated pass/fail on its own: at the
// zoom levels reachable from a 2.4 MHz recording a cell can be only a few screen pixels tall, and
// the ink's 5px-pitch pattern needs several rows of vertical room per cell to show at all reliably
// — this repo's own precedent (`canvas-journey.e2e.mjs`'s `LIVE_EDGE_ZONE`) is to report a
// texture-shaped signal that is real but amplitude-variable, and gate on the sturdier claims. The
// HARD gates are: (a) the server's own `coverage`/`shadow` planes, replayed from the pane's own
// requests, and (b) pixel brightness, relatively compared. Ink share is printed every time.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");
const PORT = 8807; // not in FORBIDDEN (backend.mjs); startBackend steps past it if another run holds it.

const PANE_ROW = '.hk-surface-viewport[data-viewport="pane"]';
const PANE_ACTION = `${PANE_ROW} .hk-surface-action:not([hidden])`;
// T-529 added `window`: a retune to a region is now ONE device action carrying centre and span
// together (`POST /api/control/window`) instead of a `rate` post followed by a `center` one. It
// belongs here for both of this file's uses. In `assertNoDeviceCalls` its absence silently WEAKENED
// the control — a pure-view pan that wrongly commanded the radio through the new route would not
// have been seen. And below, it is what a "centre change" now looks like.
const DEVICE = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)$/;
const ZOOM = { shift: true }; // frequency-only (T-472): every claim below is about frequency.
const ZOOM_IN_DELTA = -400;
const ZOOM_OUT_DELTA = 600;

// The minimap strip (bottom) and spectrum-trace strip (top of each pane), as in canvas-journey.e2e.mjs.
const MINIMAP_PX = 110;
const TRACE_PX = 96;

// Band A: the recording's own live FM signal (pilot + RDS), strong and real. Band B: far outside
// the 2.4 MHz recording, noise-floor-served but a genuine capture. Band C: never touched.
const A_HZ = 101.3e6, A_VIEW_SPAN_HZ = 250e3;
const B_HZ = 433.92e6;
const C_HZ = 200.0e6, C_VIEW_SPAN_HZ = 250e3;

// ---------------------------------------------------------------------------
// Reading the page
// ---------------------------------------------------------------------------

const ROWS = `JSON.stringify([...document.querySelectorAll('.hk-surface-viewport')].map((v) => {
  const b = v.querySelector('.hk-surface-action');
  const ruler = v.querySelector('.hk-surface-ruler');
  return {
    id: v.querySelector('.hk-surface-id')?.textContent ?? '',
    viewport: v.getAttribute('data-viewport'),
    where: v.querySelector('.hk-surface-where')?.textContent ?? '',
    counts: v.querySelector('.hk-surface-counts')?.textContent ?? '',
    hasButton: !!b && !b.hidden,
    disabled: b ? b.disabled : null,
    why: v.querySelector('.hk-surface-why')?.textContent ?? '',
    ruler: (ruler && !ruler.hidden) ? (ruler.textContent ?? '') : '',
  };
}))`;
const rows = async (page) => JSON.parse(await page.eval(ROWS));
const pane0 = async (page) => {
  const r = (await rows(page)).filter((x) => x.viewport === "pane")[0];
  assert.ok(r, "the page is drawing no pane at all");
  return r;
};

/** The pane's frequency window, parsed from `.hk-surface-where` (T-478: numbers, never the string). */
function windowOf(where) {
  const m = /^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/.exec(where);
  assert.ok(m, `the pane readout is not a frequency window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { centerHz, halfHz, loHz: centerHz - halfHz, hiHz: centerHz + halfHz, spanHz: 2 * halfHz };
}
const MHz = (hz) => (hz / 1e6).toFixed(4);
const spanOf = (v) => `${MHz(v.loHz)}-${MHz(v.hiHz)} MHz (± ${MHz(v.halfHz)})`;

async function get(backend, p) {
  const r = await fetch(`${backend.origin}${p}`, { headers: { authorization: `Bearer ${backend.token}` } });
  assert.ok(r.ok, `GET ${p} -> ${r.status}`);
  return r.json();
}

/** The capture window the mock front end reports it is using, right now. */
async function tunedWindow(backend) {
  const nav = await get(backend, "/api/navigation");
  const w = nav.windows?.[0];
  assert.ok(w, `the mock reported no capture window: ${JSON.stringify(nav.windows)}`);
  return { loHz: w.f_lo_hz, hiHz: w.f_hi_hz, spanHz: w.span_hz, centerHz: w.center_hz };
}

/**
 * Poll the SERVER's own coverage for one band **over the window since `sinceS`** until it reports
 * `observed` for at least `want` of the cells where the answer is known, and return what it last
 * said.
 *
 * **Why this replaces the two tuned sleeps this file used to carry** (T-690). Both of them —
 * "let history accumulate at B" (3 s) and "let real capture time accumulate at the re-swept band"
 * (10 s, tuned up from 3 s when 3 s measured 7/26 = 27 % against a 30 % gate) — are wall-clock
 * budgets for an amount of CAPTURE. How much capture happens in ten seconds is a property of the
 * machine, not of the product: on a contended box the mock's ingest, the fold and the tile route
 * all run slower, and the same sleep buys a fraction of the rows. A budget in the wrong unit is
 * exactly what this repo keeps finding filed as "flake".
 *
 * The window is scoped to `[sinceS, now]` and not to all time, which is the whole point: band A
 * was observed at the start of this run, so an unscoped query would answer "observed" for it
 * forever and the wait would be a no-op. Scoped, it asks the only question that is a premise —
 * *has the radio observed here SINCE it was sent here* — and `unknown` is kept out of the
 * denominator for the same reason `canvas-journey.e2e.mjs`'s `coverage` keeps it out.
 *
 * It is a PREMISE, not the claim: the pixel claims that follow (shadow is dimmer than live;
 * re-swept is brighter than shadow) are untouched and still carry the whole of ADR-0020. It
 * reports rather than throws, so a radio that genuinely never resumed observing still reaches the
 * caller's assertion and fails there with the measured number.
 */
async function waitForObservedSince(backend, targetHz, sinceS, { want = 0.5, timeoutMs = 60000, everyMs = 500 } = {}) {
  const t0 = Date.now();
  let last = { observed: 0, known: 0, share: 0 };
  for (;;) {
    const q = new URLSearchParams({
      f_lo: String(Math.round(targetHz - 1e6)), f_hi: String(Math.round(targetHz + 1e6)),
      cells: "16", rows: "16",
      t0: String(Math.round(sinceS)), t1: String(Math.round(Date.now() / 1000)),
    });
    const cov = await get(backend, `/api/coverage?${q}`).catch(() => null);
    const cells = cov?.any?.cells ?? [];
    const observed = cells.filter((c) => c?.state === "observed").length;
    const unknown = cells.filter((c) => c?.state === "unknown").length;
    const known = cells.length - unknown;
    last = { observed, known, share: known ? observed / known : 0 };
    if (last.share >= want) return { ...last, ms: Date.now() - t0, reached: true };
    if (Date.now() - t0 > timeoutMs) return { ...last, ms: Date.now() - t0, reached: false };
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/** Poll until the mock has put something in the coverage map, so the page opens ON the capture. */
async function waitForCoverage(backend, timeoutMs) {
  const t0 = Date.now();
  const q = new URLSearchParams({ f_lo: "1000000", f_hi: "6000000000", cells: "128", rows: "32" });
  for (;;) {
    const cov = await get(backend, `/api/coverage?${q}`).catch(() => null);
    const observed = (cov?.any?.cells ?? []).filter((c) => c?.state === "observed").length;
    if (observed > 0) return { observed, ms: Date.now() - t0 };
    if (Date.now() - t0 > timeoutMs) throw new Error(`the mock SDR put nothing in the coverage map in ${timeoutMs} ms`);
    await new Promise((r) => setTimeout(r, 500));
  }
}

async function waitForResident(page, { timeoutMs = 20000, everyMs = 400 } = {}) {
  const t0 = Date.now();
  let last = "";
  for (;;) {
    const row = await pane0(page);
    last = row.counts;
    const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(row.counts);
    if (m && Number(m[1]) > 0 && Number(m[2]) === 0 && Number(m[3]) === 0) {
      return { resident: true, counts: last, ms: Date.now() - t0 };
    }
    if (Date.now() - t0 > timeoutMs) return { resident: false, counts: last, ms: Date.now() - t0 };
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/**
 * An upper bound on how many seconds a pane spans, read off its own ruler (T-459, the reading
 * `canvas-journey.e2e.mjs` uses): the oldest time tick's age plus the widest gap between ticks.
 * `null` when the ruler states fewer than two time ticks.
 */
function paneSpanBoundS(ruler) {
  const m = /time (.*)$/.exec(ruler ?? "");
  if (!m) return null;
  const unit = { ms: 1e-3, s: 1, m: 60, h: 3600 };
  const ages = m[1].split(",").map((x) => {
    const mm = /([\d.]+) m ([\d.]+) s/.exec(x);
    if (mm) return Number(mm[1]) * 60 + Number(mm[2]);
    const t = /([\d.]+) (ms|s|h)/.exec(x);
    return t ? Number(t[1]) * unit[t[2]] : NaN;
  }).filter(Number.isFinite).sort((a, b) => a - b);
  if (ages.length < 2) return null;
  let gap = 0;
  for (let i = 1; i < ages.length; i++) gap = Math.max(gap, ages[i] - ages[i - 1]);
  return ages[ages.length - 1] + gap;
}

/**
 * Wait until this server's record covers both the pane's own time span and a small recency margin
 * (T-507's rule, `canvas-journey.e2e.mjs`'s pattern): before that, the pane's oldest rows are time
 * before the server began recording — honestly grey, not yet a live baseline to measure against.
 * A brand-new `hk serve` over a 5 s-looping fixture needs only a few seconds; this is a safety net,
 * not the steady state, and it reports rather than throws so a slow CI box gets a clear diagnostic.
 */
async function waitForRecordToCover(page, backend, { minS = 6, timeoutMs = 30000 } = {}) {
  const t0 = Date.now();
  for (;;) {
    const ruler = (await pane0(page)).ruler;
    const paneS = paneSpanBoundS(ruler) ?? minS;
    const h = (await get(backend, `/api/coverage?f_lo=${Math.round(A_HZ - 2e6)}&f_hi=${Math.round(A_HZ + 2e6)}&cells=1`)).horizon;
    const began = h?.recording_began_s;
    if (typeof began === "number") {
      const ageS = Date.now() / 1000 - began;
      if (ageS > Math.max(paneS, minS) + 1) return { ageS, paneS, ruler, ms: Date.now() - t0 };
    }
    if (Date.now() - t0 > timeoutMs) return { ageS: null, paneS, ruler, ms: Date.now() - t0, timedOut: true };
    await new Promise((r) => setTimeout(r, 400));
  }
}

/** No device route was reached between `fromIdx` and now — the pan/wheel invariant, checked at the
 * points this file actually relies on it (CLAUDE.md: a pan or a wheel never commands the radio). */
function assertNoDeviceCalls(page, fromIdx, label) {
  const calls = page.requests.slice(fromIdx).filter((r) => DEVICE.test(new URL(r.url).pathname));
  assert.deepEqual(calls.map((r) => new URL(r.url).pathname), [],
    `${label}: a pure view gesture reached a device route`);
}

async function centre(page) {
  const r = await page.$rect(".sf-canvas");
  assert.ok(r && r.w > 300 && r.h > 260, `the canvas has no usable box: ${JSON.stringify(r)}`);
  return { x: r.x + r.w * 0.5, y: r.y + r.h * 0.35, rect: r };
}

/**
 * Pan by `offHz` at the CURRENT span, one drag, re-reading the view fresh every call rather than
 * from a cached measurement.
 *
 * **Why not a measured calibration** (the pattern `canvas-journey.e2e.mjs`'s `calibratePan` uses
 * for the small in-band moves other e2e files make). A first version of this file probed hzPerPx
 * with a small drag-and-restore, and it broke exactly once, expensively: near a frequency-axis
 * bound (this file's jumps pass close to the 1 MHz floor while zoomed out), the PROBE direction
 * landed on the clamped side (`panes.ts`'s `normalise` pins the centre, never the span, so the
 * probe read `moved: 0`) while the RESTORE direction was not clamped and silently moved the view
 * ~71 MHz — a measurement that was wrong in exactly the situation this file's big jumps create.
 * The pixel-to-Hz relationship a drag uses is not a guess to begin with: `panes.ts`'s own `drag()`
 * computes it as `dHz = -(dx / paneWidthPx) * spanHz`, so `spanHz / width` is not "re-deriving the
 * product's arithmetic" in the T-478 sense that other files' comments warn against (asserting a
 * VALUE the product should be judged against) — it is the one number that formula is already
 * built from, read off the page's own reported span and rect rather than assumed. Every step
 * re-reads both, so it can never go stale the way one calibration held across many zoom levels did.
 */
async function panByHz(page, at, offHz) {
  const view = windowOf((await pane0(page)).where);
  if (!(view.spanHz > 0)) return { moved: false };
  const dxPx = Math.max(-at.rect.w * 0.42, Math.min(at.rect.w * 0.42, -offHz * at.rect.w / view.spanHz));
  if (Math.abs(dxPx) < 1) return { moved: false };
  await page.drag(at, { x: at.x + dxPx, y: at.y }, 8);
  await page.frames(3);
  return { moved: true };
}

/**
 * Navigate the pane's VIEW — never the device — to `targetHz ± spanHz/2`, however far that is from
 * where it is looking now (this file jumps hundreds of MHz, unlike the small in-band moves other
 * e2e files make). Three phases:
 *
 *  1. Zoom OUT until the target is comfortably inside the current span — panning at the fixture's
 *     opening ~2.4 MHz span would take hundreds of drag steps to reach 433.92 MHz.
 *  2. Pan onto the target's centre AT THAT WIDE SPAN, one [[panByHz]] step at a time (re-reading
 *     span and rect fresh every step, so a bound clamp on one step cannot corrupt the next).
 *  3. Zoom IN, at the SAME screen point (`at`, the pane's own centre pixel). Because the view is
 *     symmetric about its centre and `at` sits over that centre pixel, the centre Hz does not move
 *     while span shrinks *away from any bound* — near one, `panes.ts`'s centre clamp can still
 *     shift it, which is exactly why every step below re-pans by the CURRENT offset rather than
 *     assuming zoom alone finishes the job.
 */
async function gotoFreq(page, at, targetHz, spanHz) {
  let view = windowOf((await pane0(page)).where);
  const trail = [spanOf(view)];
  for (let i = 0; i < 24; i++) {
    const dist = Math.abs(targetHz - view.centerHz);
    if (view.spanHz >= dist * 2.5 || view.spanHz >= 5.9e9) break;
    await page.wheel(at, ZOOM_OUT_DELTA, ZOOM);
    await page.frames(2);
    view = windowOf((await pane0(page)).where);
    trail.push(spanOf(view));
  }
  for (let i = 0; i < 30; i++) {
    view = windowOf((await pane0(page)).where);
    const off = targetHz - view.centerHz;
    if (Math.abs(off) <= view.spanHz * 0.03) break;
    const { moved } = await panByHz(page, at, off);
    trail.push(spanOf(windowOf((await pane0(page)).where)));
    if (!moved) break; // a step this small should not happen before the break condition above fires
  }
  for (let i = 0; i < 40; i++) {
    view = windowOf((await pane0(page)).where);
    if (view.spanHz <= spanHz * 1.3) break;
    await page.wheel(at, ZOOM_IN_DELTA, ZOOM);
    await page.frames(2);
    // Re-pan onto the target every step, not only at the end (see the header): near a frequency
    // bound the centre clamp can drift a little on each zoom-in step, and correcting it here, while
    // the span is still wide enough that a small clamp drift is a small fraction of it, is cheaper
    // and more reliable than one large correction at the final, narrow span.
    view = windowOf((await pane0(page)).where);
    const off = targetHz - view.centerHz;
    if (Math.abs(off) > view.spanHz * 0.05) await panByHz(page, at, off);
    trail.push(spanOf(windowOf((await pane0(page)).where)));
  }
  for (let i = 0; i < 10; i++) {
    view = windowOf((await pane0(page)).where);
    const off = targetHz - view.centerHz;
    if (Math.abs(off) <= view.spanHz * 0.15) break;
    const { moved } = await panByHz(page, at, off);
    trail.push(spanOf(windowOf((await pane0(page)).where)));
    if (!moved) break;
  }
  view = windowOf((await pane0(page)).where);
  return { view, trail };
}

/** Navigate onto `targetHz` and press the per-pane Retune control — the one gated `DeviceAction`
 * path (`ui/src/surface/retune.ts`), exactly as a user would. Retries the press (T-476/T-508's
 * pattern in `surface-retune.e2e.mjs`: a rate change can land the centre inside the settle gap and
 * come back `device_busy`, which is the radio being one-capture-at-a-time, not a fault). */
async function retuneTo(page, backend, at, targetHz, viewSpanHz, label) {
  const g = await gotoFreq(page, at, targetHz, viewSpanHz);
  let row = await pane0(page);
  for (let i = 0; i < 20 && row.disabled !== false; i++) { await page.frames(3); row = await pane0(page); }
  assert.equal(row.disabled, false,
    `${label}: the retune control is not takeable at ${spanOf(g.view)}: ${row.why}\ntrail: ${g.trail.join(" -> ")}`);
  const said = row.why.match(/^Retune to ([\d.]+) MHz at ([\d.]+) MHz span/);
  assert.ok(said, `${label}: the enabled control names no destination: ${row.why}`);
  const saidCenterHz = Number(said[1]) * 1e6;
  let w1 = null;
  for (let attempt = 0; attempt < 6; attempt++) {
    await page.click(`document.querySelector('${PANE_ACTION}')`);
    await page.frames(4);
    for (let i = 0; i < 24; i++) {
      w1 = await tunedWindow(backend);
      if (Math.abs(w1.centerHz - saidCenterHz) < 100) break;
      await new Promise((r) => setTimeout(r, 250));
    }
    if (w1 && Math.abs(w1.centerHz - saidCenterHz) < 100) break;
  }
  assert.ok(w1 && Math.abs(w1.centerHz - saidCenterHz) < 100,
    `${label}: the radio never reached ${saidCenterHz} Hz (last window centre ${w1?.centerHz})`);
  return { view: g.view, said: { centerHz: saidCenterHz }, tuned: w1 };
}

// ---------------------------------------------------------------------------
// Reading the SERVER's own planes — from the pane's own requests, never hand-addressed
// ---------------------------------------------------------------------------

/**
 * Replay the most recent `/api/tiles` request the PANE itself made whose answer covers `targetHz`
 * — the exact response that drew the pixels on screen, re-fetched fresh so its `coverage`/`shadow`
 * are current. `maxSpanHz` excludes the minimap's own (much wider) tile requests: every window this
 * file views is under 2 MHz; 80 MHz is comfortably narrower than anything the minimap asks for
 * (which spans toward the whole 1 MHz-6 GHz surface) and comfortably wider than a pane's.
 */
async function findPaneTileFor(page, backend, targetHz, { sinceIdx = 0, maxSpanHz = 80e6 } = {}) {
  const reqs = page.requests.slice(sinceIdx).filter((r) => r.url.includes("/api/tiles"));
  const why = { refused: 0, failed: 0, shapeless: 0, tooWide: 0, elsewhere: 0 };
  for (let i = reqs.length - 1; i >= 0; i--) {
    let json = null;
    // **A 503 is the route saying "ask again", not "this tile does not cover your band"** (T-690).
    // `/api/tiles` takes its slot before it does any work and the page itself holds up to four of
    // them, so this replay is a FIFTH reader of a shared budget — and it was skipping every
    // candidate the route happened to be busy for, then reporting the absence as "no pane tile
    // response covers band A". Observed exactly that way in a 13-spec run, 8 s into the file, on a
    // route this file's own page was saturating. So a refusal is retried on a capped cadence, the
    // same answer the product's own bootstrap gives it, and the reasons are counted so an honest
    // absence and a route that was busy can never again be reported with the same words.
    for (let attempt = 0; attempt < 12; attempt++) {
      let resp = null;
      try {
        resp = await fetch(reqs[i].url, { headers: { authorization: `Bearer ${backend.token}` } });
      } catch { break; }
      if (resp.status === 503) { await new Promise((r) => setTimeout(r, Math.min(1000, 100 * 2 ** attempt))); continue; }
      if (!resp.ok) break;
      try { json = await resp.json(); } catch { json = null; }
      break;
    }
    if (!json) { why.refused++; continue; }
    const g = json.grid;
    if (!g || !(g.nf > 0) || !(g.f_cell_hz > 0)) { why.shapeless++; continue; }
    const spanHz = g.nf * g.f_cell_hz;
    if (spanHz > maxSpanHz) { why.tooWide++; continue; }
    if (targetHz < g.f_lo_hz || targetHz >= g.f_lo_hz + spanHz) { why.elsewhere++; continue; }
    return { url: reqs[i].url, json, spanHz };
  }
  return { none: true, tried: reqs.length, why };
}

/** `findPaneTileFor`'s answer, or a failure that says WHICH reason it ran out of. */
function tileOrWhy(found, label) {
  assert.ok(found && !found.none,
    `${label}. Of ${found?.tried ?? 0} pane tile request(s) replayed: ` +
    `${found?.why?.refused ?? 0} the route would not answer even after retrying its 503, ` +
    `${found?.why?.shapeless ?? 0} carried no usable grid, ${found?.why?.tooWide ?? 0} were wider ` +
    `than a pane's, ${found?.why?.elsewhere ?? 0} covered other spectrum. A route that was busy ` +
    "and a band that was never drawn are different findings.");
  return found;
}

/** Every cell of a coverage RLE plane, decoded once: `states[code]` per (row, col), row-major, row
 * 0 earliest — exactly docs/api.md's `coverage` encoding. Carries its own `t0_s`/`t_cell_s` too:
 * a tile's time extent can be MUCH taller than however much real capture has actually happened
 * (this fixture's finest level is 256 rows x 1 s = 256 s tall, filled from row 0 upward as capture
 * progresses), so "row `nt - 1`" is very often still in the future, not "the newest capture". */
function decodeCoveragePlane(coverageBlock) {
  const { grid, planes, selected, states } = coverageBlock;
  const plane = planes[selected.plane];
  const nt = grid.nt, nf = grid.nf;
  const out = new Array(nt * nf);
  let idx = 0;
  for (let i = 0; i < plane.runs.length; i += 2) {
    const code = plane.runs[i], count = plane.runs[i + 1], state = states[code];
    for (let k = 0; k < count; k++) out[idx++] = state;
  }
  assert.equal(idx, nt * nf, `coverage plane runs summed to ${idx} cells, expected ${nt * nf}`);
  return {
    nt, nf, f_lo_hz: grid.f_lo_hz, f_cell_hz: grid.f_cell_hz, t0_s: grid.t0_s, t_cell_s: grid.t_cell_s,
    at: (row, col) => out[row * nf + col],
  };
}

/** The row index that holds `atS` on a `(t0_s, t_cell_s)` axis, clamped into `[0, nt - 1]`. Row 0
 * is earliest (docs/api.md's `coverage` and `shadow` blocks both say so). */
function rowAt(t0_s, t_cell_s, nt, atS) {
  return Math.min(nt - 1, Math.max(0, Math.floor((atS - t0_s) / t_cell_s)));
}

/**
 * How a small window of rows AT THE LIVE EDGE reads for one frequency column — "at the live edge"
 * meaning the rows around `edgeS` (the store's newest frame; `shadow.edge_s`), not `nt - 1`. See
 * `decodeCoveragePlane`'s header for why the two are not the same thing on this fixture.
 */
function edgeRowsCoverage(cov, targetHz, edgeS, frac = 0.1) {
  const col = Math.min(cov.nf - 1, Math.max(0, Math.floor((targetHz - cov.f_lo_hz) / cov.f_cell_hz)));
  const edgeRow = rowAt(cov.t0_s, cov.t_cell_s, cov.nt, edgeS);
  const n = Math.max(1, Math.round(cov.nt * frac));
  const from = Math.max(0, edgeRow - n + 1), to = edgeRow;
  let observed = 0, unobserved = 0, unknown = 0;
  for (let row = from; row <= to; row++) {
    const s = cov.at(row, col);
    if (s === "observed") observed++; else if (s === "unobserved") unobserved++; else unknown++;
  }
  const total = to - from + 1;
  return { col, edgeRow, from, to, total, observed, unobserved, unknown,
    observedShare: total ? observed / total : 0, unobservedShare: total ? unobserved / total : 0 };
}

/** Whether the `shadow` plane carries a run over a small window of rows AT THE LIVE EDGE for one
 * frequency column — "the grid's axes, row 0 earliest" (docs/api.md), so this uses `tile.grid`,
 * not `coverage.grid` (the two can differ in resolution; the shadow's column/row indices are
 * always the outer grid's). */
function shadowNearEdge(shadow, grid, targetHz, edgeS, frac = 0.1) {
  const col = Math.min(grid.nf - 1, Math.max(0, Math.floor((targetHz - grid.f_lo_hz) / grid.f_cell_hz)));
  const edgeRow = rowAt(grid.t0_s, grid.t_cell_s, grid.nt, edgeS);
  const n = Math.max(1, Math.round(grid.nt * frac));
  const from = Math.max(0, edgeRow - n + 1), to = edgeRow;
  const hits = [];
  for (let i = 0; i < shadow.runs; i++) {
    if (shadow.f[i] !== col) continue;
    const r0 = shadow.row[i], r1 = r0 + shadow.rows[i];
    if (r1 > from && r0 <= to) hits.push({ row: r0, rows: shadow.rows[i], lastDb: shadow.last_db[i], lastTS: shadow.last_t_s[i] });
  }
  return { col, edgeRow, from, to, hits };
}

// ---------------------------------------------------------------------------
// Harvesting the grey/ink colours from the product's OWN rendering (never hardcoded)
// ---------------------------------------------------------------------------

async function readSwatchRgba(page, mark) {
  const json = await page.eval(`(() => {
    const c = document.querySelector('.sp-legend-row[data-mark="${mark}"] canvas');
    if (!c) return null;
    const ctx = c.getContext('2d');
    const img = ctx.getImageData(0, 0, c.width, c.height);
    return JSON.stringify({ w: c.width, h: c.height, data: Array.from(img.data) });
  })()`);
  assert.ok(json, `no legend swatch canvas for mark="${mark}"`);
  return JSON.parse(json);
}

/** The single flat colour of the "unobserved" swatch — THE grey, however the product defines it
 * today (`CELL_MARKS[UNOBSERVED]`, `ui/src/surface/cellrule.ts`). Any pixel gives it; asserted
 * uniform as a sanity check on the instrument itself. */
function greyFromSwatch({ w, h, data }) {
  const [r, g, b] = [data[0], data[1], data[2]];
  for (let i = 0; i < w * h * 4; i += 4) {
    assert.ok(data[i] === r && data[i + 1] === g && data[i + 2] === b,
      `the "unobserved" legend swatch is not a flat colour at pixel ${i / 4}: expected [${r},${g},${b}]`);
  }
  return [r, g, b];
}

function columnColours({ w, h, data }, col) {
  const set = new Set();
  for (let row = 0; row < h; row++) {
    const i = (row * w + col) * 4;
    set.add((data[i] << 16) | (data[i + 1] << 8) | data[i + 2]);
  }
  return set;
}

/**
 * The shadow's ink colour: the ONE colour common to every ramp position.
 *
 * A shadow cell alternates ground (`cmap(x) * gain`, which varies with the ramp position `x`) and
 * ink (a fixed colour, independent of `x` — `SHADOW_MARK.ink`). So across three columns at
 * different `x` (0, 0.5, 1 — different points on the ramp, so their ground colours must differ),
 * the ink is exactly the colour every one of them has in common. This needs no assumption about
 * which of the two is the majority within a column (a first version assumed the ink was always the
 * minority of a 5px cycle and got it backwards on the ramp's near-white top, where the DIMMED
 * ground rounds close to the ink's own darkness).
 */
function inkFromSwatch({ w, h, data }) {
  const img = { w, h, data };
  const cols = [0, Math.floor((w - 1) / 2), w - 1];
  const sets = cols.map((c) => columnColours(img, c));
  let common = sets[0];
  for (const s of sets.slice(1)) common = new Set([...common].filter((k) => s.has(k)));
  assert.equal(common.size, 1,
    `expected exactly one colour common to every sampled ramp position (the ink), got ${common.size}: ` +
    `${JSON.stringify([...common].map((k) => [(k >> 16) & 255, (k >> 8) & 255, k & 255]))}`);
  const [k] = common;
  return [(k >> 16) & 255, (k >> 8) & 255, k & 255];
}

/** Open `/surface.html` against the same backend (its legend, not its retune: the preview page
 * never imports `retune.ts`, so this is read-only) and harvest the two colours. The tab is left
 * open (closed with the rest of the browser at the end of the test) — `harness.mjs`'s `Page` has
 * no per-tab close, only `Browser.close()`. */
async function harvestMarks(browser, backend) {
  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/surface.html#token=${backend.token}`), "load");
  await page.waitForSurfaceMounted();
  await page.waitFor("the legend to mount its unobserved and shadow swatches",
    `!!document.querySelector('.sp-legend-row[data-mark="unobserved"] canvas') &&
     !!document.querySelector('.sp-legend-row[data-mark="shadow"] canvas')`, { timeoutMs: 15000 });
  const grey = greyFromSwatch(await readSwatchRgba(page, "unobserved"));
  const ink = inkFromSwatch(await readSwatchRgba(page, "shadow"));
  return { greyRgb: grey, inkRgb: ink };
}

// ---------------------------------------------------------------------------
// Pixels
// ---------------------------------------------------------------------------

function paneRectOf(rect, dpr) {
  const paneH = rect.h * dpr - MINIMAP_PX;
  const traceH = Math.max(0, Math.min(TRACE_PX, Math.floor(paneH / 3)));
  return { x: rect.x, w: rect.w, y: rect.y + traceH / dpr, h: (paneH - traceH) / dpr };
}
async function paneGeometry(page) {
  const rect = await page.$rect(".sf-canvas");
  assert.ok(rect && rect.w > 300 && rect.h > 260, `the canvas has no usable box: ${JSON.stringify(rect)}`);
  const dpr = await page.eval("window.devicePixelRatio || 1");
  return { rect, dpr, pane: paneRectOf(rect, dpr) };
}
/** The pane's data rect, inset a few pixels clear of every edge (a boundary pixel is a rounding
 * question, not a colour question — `surface-colour.e2e.mjs`'s `INSET`). */
function roiOf(pane, inset = 8) {
  return { x: pane.x + inset, y: pane.y + inset, w: Math.max(1, pane.w - 2 * inset), h: Math.max(1, pane.h - 2 * inset) };
}

function inspect(img, rect, greyRgb, inkRgb, tol = 2) {
  const near = (d, i, rgb) => Math.abs(d[i] - rgb[0]) <= tol && Math.abs(d[i + 1] - rgb[1]) <= tol && Math.abs(d[i + 2] - rgb[2]) <= tol;
  const x0 = Math.max(0, Math.round(rect.x)), y0 = Math.max(0, Math.round(rect.y));
  const x1 = Math.min(img.width, Math.round(rect.x + rect.w)), y1 = Math.min(img.height, Math.round(rect.y + rect.h));
  let n = 0, grey = 0, ink = 0;
  for (let y = y0; y < y1; y++) {
    for (let x = x0; x < x1; x++) {
      const d = (y * img.width + x) * 4;
      n++;
      if (near(img.data, d, greyRgb)) grey++;
      if (near(img.data, d, inkRgb)) ink++;
    }
  }
  return { n, grey, ink, greyShare: n ? grey / n : 0, inkShare: n ? ink / n : 0, census: census(img, rect) };
}

/**
 * Let tiles arrive and the frame settle, then shoot.
 *
 * **The settle is the PAGE's own statement that it has drawn, with the constant only as a floor**
 * (T-690). A fixed 2200 ms is a bet on the tile route's service rate: measured across this repo's
 * own runs that rate moves by more than twenty-fold between a quiet box and a full suite, so the
 * bet decides whether the census below reads the product's answer or reads how far the fetch had
 * got. The pane already publishes the answer — `· 0 pending` in its own chrome — so that is what
 * is waited on. Coarse stand-ins are NOT waited out (surface-nav's rule, T-564): a stand-in is a
 * real ancestor tile with real cells in it, so a pane drawn with some is drawing. `pending` is the
 * pane's own statement that part of what is on screen is its bare ground, and that is the state no
 * pixel claim here may be measured in.
 *
 * It reports rather than throws — the caller's assertion is the right place for "the pane never
 * drew", with the counts in it.
 */
async function draw(page, { settleMs = 600, timeoutMs = 25000 } = {}) {
  await page.frames(4);
  const counts = `(document.querySelector('${PANE_ROW} .hk-surface-counts')?.textContent ?? '')`;
  const drew = await page.waitFor("the pane to report itself drawn (0 pending)",
    `/· 0 pending/.test(${counts}) && !/^0 tiles/.test(${counts})`, { timeoutMs })
    .then(() => true, () => false);
  // A short settle after the page says it is drawn: the last upload and the frame that uses it are
  // not the same tick. A floor, not a budget — it is not waiting for the route.
  await new Promise((r) => setTimeout(r, settleMs));
  await page.frames(4);
  const img = await page.shot();
  img.drew = drew;
  img.counts = await page.eval(counts);
  return img;
}

// ---------------------------------------------------------------------------
// The journey: one mock backend, one browser, one app page
// ---------------------------------------------------------------------------

test("T-521: sweep then leave = shadow, never swept = grey, re-sweep = bright — through the mock SDR", async (t) => {
  const backend = await startBackend({ port: PORT, mockDevice: true });
  t.after(() => backend.stop());
  const covered = await waitForCoverage(backend, 90000);
  t.diagnostic(`mock SDR coverage after ${covered.ms} ms: ${covered.observed} observed cells`);

  const browser = await Browser.open();
  t.after(() => browser.close());

  // ——— Harvest the grey/ink colours from the product's own legend, not hardcoded (see header). ———
  const marks = await harvestMarks(browser, backend);
  t.diagnostic(`harvested from the legend: grey rgb(${marks.greyRgb.join(",")}), shadow ink rgb(${marks.inkRgb.join(",")})`);
  assert.notDeepEqual(marks.greyRgb, marks.inkRgb, "the grey and the shadow ink are the same colour — the instrument found nothing to tell apart");

  const page = await browser.page();
  assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
  await page.waitFor("the app shell to mount its surface slot", `!!document.querySelector('.sf-canvas')`, { timeoutMs: 15000 });
  await page.waitFor("the app's surface to finish addressing",
    `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
  const note = (await page.$text(".sf-note")) ?? "";
  assert.ok(!/could not be addressed|WebGL2 is unavailable/.test(note), `the surface refused to mount: ${note}`);
  await page.waitFor("the per-pane retune control to be on the page", `!!document.querySelector('${PANE_ACTION}')`, { timeoutMs: 20000 });
  await page.frames(4);

  const at = await centre(page);

  // ===========================================================================
  // PHASE 1 — band A is live at boot (the recording's own signal): a baseline.
  // ===========================================================================
  const openIdx = page.requests.length;
  const gA0 = await gotoFreq(page, at, A_HZ, A_VIEW_SPAN_HZ);
  t.diagnostic(`opened onto band A: ${spanOf(gA0.view)} (${gA0.trail.length} nav step(s))`);
  assertNoDeviceCalls(page, openIdx, "panning onto band A at boot");
  await waitForResident(page);
  const wr0 = await waitForRecordToCover(page, backend);
  t.diagnostic(`record covers the pane after ${wr0.ms} ms (age ${wr0.ageS?.toFixed?.(1) ?? "?"} s, pane span bound ${wr0.paneS?.toFixed?.(1) ?? "?"} s, ruler "${wr0.ruler}")${wr0.timedOut ? " — TIMED OUT, measuring anyway" : ""}`);
  const gA = await paneGeometry(page);
  const roiA = roiOf(gA.pane);
  const imgA0 = await draw(page);
  const liveA = inspect(imgA0, roiA, marks.greyRgb, marks.inkRgb);
  t.diagnostic(`LIVE A baseline: meanLuma ${liveA.census.meanLuma.toFixed(1)}, grey ${(liveA.greyShare * 100).toFixed(1)}%, ` +
    `ink ${(liveA.inkShare * 100).toFixed(1)}%, distinct ${liveA.census.distinct} · drawn with "${imgA0.counts}"` +
    (imgA0.drew ? "" : " — THE PANE NEVER REPORTED ITSELF DRAWN"));
  assert.ok(liveA.census.distinct >= 4, `band A at boot is not a real render (only ${liveA.census.distinct} distinct colours) — this run proves nothing`);
  assert.ok(liveA.greyShare < 0.5, `band A at boot already reads mostly grey (${(liveA.greyShare * 100).toFixed(1)}%) — this run has no live baseline to lose`);

  const tileA0 = tileOrWhy(await findPaneTileFor(page, backend, A_HZ, { sinceIdx: 0 }),
    "no pane tile response covers band A while it is live — cannot establish the server-side baseline");
  {
    const cov = decodeCoveragePlane(tileA0.json.coverage);
    const nr = edgeRowsCoverage(cov, A_HZ, tileA0.json.shadow.edge_s);
    t.diagnostic(`SERVER band A (live, pre-move): edge row ${nr.edgeRow}, window [${nr.from},${nr.to}] observed ${nr.observed}/${nr.total}, unobserved ${nr.unobserved}`);
    assert.ok(nr.observedShare > 0.5, `band A's own tile does not report itself mostly observed near the live edge while live: ${JSON.stringify(nr)}`);
  }

  // ===========================================================================
  // PHASE 2 — move away: retune to band B, far outside the recording.
  // ===========================================================================
  const toB = await retuneTo(page, backend, at, B_HZ, 200e3, "move to B");
  // The premise window opens where the RADIO arrived, not where the navigation started: `retuneTo`
  // returns only once the mock reports the new centre, so rows before this instant are rows the
  // front end was still somewhere else for and would only dilute the share.
  const sinceB = Date.now() / 1000;
  t.diagnostic(`retuned away: said ${MHz(toB.said.centerHz)} MHz, radio now at ${MHz(toB.tuned.centerHz)} ± ${MHz(toB.tuned.spanHz / 2)} MHz`);
  assert.ok(Math.abs(toB.tuned.centerHz - A_HZ) > 100e6, "the retune to band B did not actually leave band A's neighbourhood");
  await waitForResident(page);
  // Wait for the SERVER to say the radio is really producing at B — the premise "we moved away and
  // are now recording somewhere else" — rather than sleeping a constant and hoping (T-690). The
  // shadow search over band A needs a live edge that has moved on; this is that, measured.
  const atB = await waitForObservedSince(backend, B_HZ, sinceB, { want: 0.2 });
  t.diagnostic(`band B observed by the server since the retune, after ${atB.ms} ms: ` +
    `${atB.observed}/${atB.known} known cells` +
    (atB.reached ? "" : " — NEVER REACHED the premise, measuring anyway"));

  // ===========================================================================
  // PHASE 3 — CLAIM 1: band A, now unobserved, renders SHADOW — not grey, dimmer than live.
  // ===========================================================================
  const sinceMoveIdx = page.requests.length;
  await gotoFreq(page, at, A_HZ, A_VIEW_SPAN_HZ); // a VIEW pan only, never a device call
  assertNoDeviceCalls(page, sinceMoveIdx, "panning back to look at departed band A");
  await waitForResident(page);
  const gA2 = await paneGeometry(page);
  const imgA1 = await draw(page);
  const shadowA = inspect(imgA1, roiOf(gA2.pane), marks.greyRgb, marks.inkRgb);
  t.diagnostic(`SHADOW A (departed): meanLuma ${shadowA.census.meanLuma.toFixed(1)}, grey ${(shadowA.greyShare * 100).toFixed(1)}%, ` +
    `ink ${(shadowA.inkShare * 100).toFixed(1)}%, distinct ${shadowA.census.distinct} · drawn with "${imgA1.counts}"` +
    (imgA1.drew ? "" : " — THE PANE NEVER REPORTED ITSELF DRAWN"));

  const tileA1 = tileOrWhy(await findPaneTileFor(page, backend, A_HZ, { sinceIdx: sinceMoveIdx }),
    "no pane tile response covers band A after the move — cannot check the server's shadow plane");
  const covA1 = decodeCoveragePlane(tileA1.json.coverage);
  const edgeS1 = tileA1.json.shadow.edge_s;
  const nrA1 = edgeRowsCoverage(covA1, A_HZ, edgeS1);
  const shA1 = shadowNearEdge(tileA1.json.shadow, tileA1.json.grid, A_HZ, edgeS1);
  t.diagnostic(`SERVER band A (departed): edge row ${nrA1.edgeRow}, window [${nrA1.from},${nrA1.to}] unobserved ${nrA1.unobserved}/${nrA1.total}; ` +
    `shadow run(s) over that span: ${JSON.stringify(shA1.hits)}`);

  // THE CLAIM, server-side: departed band A is UNOBSERVED (the radio really left) and the shadow
  // plane carries a real, finite last-known value for it.
  assert.ok(nrA1.unobservedShare > 0.5,
    `band A's own tile does not report itself unobserved after moving away: ${JSON.stringify(nrA1)}`);
  assert.ok(shA1.hits.length > 0,
    `the server's shadow plane carries NO run over band A after it was swept and departed — ADR-0020's last-known tier did not fire: ${JSON.stringify(shA1)}`);
  for (const h of shA1.hits) {
    assert.ok(Number.isFinite(h.lastDb), `a shadow run over band A carries a non-finite last_db: ${JSON.stringify(h)}`);
    assert.ok(Number.isFinite(h.lastTS) && h.lastTS <= Date.now() / 1000 + 5,
      `a shadow run over band A carries an implausible last_t_s: ${JSON.stringify(h)}`);
  }

  // THE CLAIM, pixel-side: not grey (it is not the "never looked" claim), and dimmer than it was
  // live (the ceiling defence: `SHADOW_MARK`, T-520 — no shadow pixel may be brighter than a live
  // one). Relative, not against a hardcoded gain: see this file's header.
  assert.ok(shadowA.greyShare < 0.5,
    `departed band A reads mostly THE grey (${(shadowA.greyShare * 100).toFixed(1)}%) — it should read as shadow, a real measurement, not "never observed": ${JSON.stringify(shadowA.census)}`);
  assert.ok(shadowA.census.meanLuma < liveA.census.meanLuma * 0.9,
    `departed band A (meanLuma ${shadowA.census.meanLuma.toFixed(1)}) is not meaningfully dimmer than it was live ` +
    `(meanLuma ${liveA.census.meanLuma.toFixed(1)}) — the shadow ceiling is not visibly in effect`);

  // ===========================================================================
  // PHASE 4 — CLAIM 2: band C, never swept at all, stays GREY (and carries NO shadow run at all).
  // ===========================================================================
  const sinceCIdx = page.requests.length;
  const gC = await gotoFreq(page, at, C_HZ, C_VIEW_SPAN_HZ);
  t.diagnostic(`panned to band C: ${spanOf(gC.view)}`);
  assertNoDeviceCalls(page, sinceCIdx, "panning to never-swept band C");
  await waitForResident(page);
  const gCgeom = await paneGeometry(page);
  const imgC = await draw(page);
  const greyC = inspect(imgC, roiOf(gCgeom.pane), marks.greyRgb, marks.inkRgb);
  t.diagnostic(`GREY C (never swept): meanLuma ${greyC.census.meanLuma.toFixed(1)}, grey ${(greyC.greyShare * 100).toFixed(1)}%, ` +
    `ink ${(greyC.inkShare * 100).toFixed(1)}%, distinct ${greyC.census.distinct} · drawn with "${imgC.counts}"` +
    (imgC.drew ? "" : " — THE PANE NEVER REPORTED ITSELF DRAWN"));

  const tileC = tileOrWhy(await findPaneTileFor(page, backend, C_HZ, { sinceIdx: sinceCIdx }),
    "no pane tile response covers band C — cannot check it against the server");
  const covC = decodeCoveragePlane(tileC.json.coverage);
  const nrC = edgeRowsCoverage(covC, C_HZ, tileC.json.shadow.edge_s);
  t.diagnostic(`SERVER band C: edge row ${nrC.edgeRow}, window [${nrC.from},${nrC.to}] unobserved ${nrC.unobserved}/${nrC.total}; whole-tile shadow.runs = ${tileC.json.shadow.runs}`);

  // THE ANTI-CASE, server-side: band C is unobserved, AND THE SHADOW PLANE IS EMPTY over this whole
  // tile — not merely absent at this one column. This is the guard against a shadow ever bleeding
  // onto spectrum this run's radio never looked at.
  assert.ok(nrC.unobservedShare > 0.9, `band C's own tile does not report itself unobserved: ${JSON.stringify(nrC)}`);
  assert.equal(tileC.json.shadow.runs, 0,
    `band C — never tuned to in this run — carries a SHADOW RUN. A shadow appeared over spectrum ` +
    `the radio never looked at: ${JSON.stringify(tileC.json.shadow)}`);

  // THE ANTI-CASE, pixel-side: reads as flat grey, and carries none of the ink.
  assert.ok(greyC.greyShare > 0.9,
    `band C (never swept) does not read as mostly grey (${(greyC.greyShare * 100).toFixed(1)}%): ${JSON.stringify(greyC.census)}`);
  assert.ok(greyC.inkShare < 0.02,
    `band C (never swept) shows the shadow's ink pattern (${(greyC.inkShare * 100).toFixed(1)}%) — a shadow drawn where nothing was ever observed`);
  assert.ok(greyC.census.meanLuma < shadowA.census.meanLuma,
    `band C (never observed, meanLuma ${greyC.census.meanLuma.toFixed(1)}) is not darker than departed band A ` +
    `(meanLuma ${shadowA.census.meanLuma.toFixed(1)}) — grey and shadow are not visually distinct`);

  // ===========================================================================
  // PHASE 5 — CLAIM 3: re-sweep band A. It returns to full brightness.
  // ===========================================================================
  const sinceReswIdx = page.requests.length;
  const back = await retuneTo(page, backend, at, A_HZ, A_VIEW_SPAN_HZ, "re-sweep A");
  const sinceResweep = Date.now() / 1000;
  t.diagnostic(`re-swept: said ${MHz(back.said.centerHz)} MHz, radio now at ${MHz(back.tuned.centerHz)} ± ${MHz(back.tuned.spanHz / 2)} MHz`);
  assert.ok(Math.abs(back.tuned.centerHz - A_HZ) < 3e6, `re-sweeping did not bring the radio back near band A: ${JSON.stringify(back.tuned)}`);
  await waitForResident(page);
  // `edgeRowsCoverage` below looks at a real-seconds-wide window ending at the tile's own fold
  // horizon (`shadow.edge_s`), so it needs real capture at the re-swept band to have accumulated
  // and been folded. That used to be a 10 s sleep, tuned up from 3 s when 3 s measured 7/26 = 27 %
  // against the 30 % gate — a wall-clock budget for an amount of capture, which is the wrong unit
  // the moment the box is busy (T-690). Wait on the quantity instead, from the server, bounded.
  const backAgain = await waitForObservedSince(backend, A_HZ, sinceResweep, { want: 0.5 });
  t.diagnostic(`band A observed again by the server since the re-sweep, after ${backAgain.ms} ms: ` +
    `${backAgain.observed}/${backAgain.known} known cells` +
    (backAgain.reached ? "" : " — NEVER REACHED the premise, measuring anyway"));
  const gA3 = await paneGeometry(page);
  const imgA2 = await draw(page);
  const reswptA = inspect(imgA2, roiOf(gA3.pane), marks.greyRgb, marks.inkRgb);
  t.diagnostic(`RE-SWEPT A: meanLuma ${reswptA.census.meanLuma.toFixed(1)}, grey ${(reswptA.greyShare * 100).toFixed(1)}%, ` +
    `ink ${(reswptA.inkShare * 100).toFixed(1)}%, distinct ${reswptA.census.distinct} · drawn with "${imgA2.counts}"` +
    (imgA2.drew ? "" : " — THE PANE NEVER REPORTED ITSELF DRAWN"));

  const tileA2 = tileOrWhy(await findPaneTileFor(page, backend, A_HZ, { sinceIdx: sinceReswIdx }),
    "no pane tile response covers band A after re-sweeping");
  const covA2 = decodeCoveragePlane(tileA2.json.coverage);
  const nrA2 = edgeRowsCoverage(covA2, A_HZ, tileA2.json.shadow.edge_s);
  t.diagnostic(`SERVER band A (re-swept): edge row ${nrA2.edgeRow}, window [${nrA2.from},${nrA2.to}] observed ${nrA2.observed}/${nrA2.total}`);

  // THE CLAIM, server-side: the FRESHEST rows over band A are observed AGAIN — a direct measurement,
  // not a carried-forward one.
  assert.ok(nrA2.observedShare > 0.3,
    `re-swept band A's newest rows do not read as freshly observed: ${JSON.stringify(nrA2)}`);

  // THE CLAIM, pixel-side: brighter than it was in shadow, by a clear margin — the round trip
  // closes. (Not asserted equal to the ORIGINAL live baseline: a fresh capture instant measures
  // different, real dB values, and the claim is "full brightness restored", not "identical pixels".)
  assert.ok(reswptA.census.meanLuma > shadowA.census.meanLuma * 1.15,
    `re-swept band A (meanLuma ${reswptA.census.meanLuma.toFixed(1)}) is not clearly brighter than it was in shadow ` +
    `(meanLuma ${shadowA.census.meanLuma.toFixed(1)}) — re-sweeping did not restore full brightness`);
  assert.ok(reswptA.greyShare < 0.5,
    `re-swept band A reads mostly grey (${(reswptA.greyShare * 100).toFixed(1)}%) — it should read live again`);

  // ===========================================================================
  // THE CONTROL: `assertNoDeviceCalls` above already pinned the two pure-view legs (panning back
  // onto departed band A, and onto never-swept band C) to zero device calls. This is the other
  // half: retuning DID happen — at least the two presses this file made (away to B, back to A) —
  // so the shadow/grey/live states above are not an accident of a front end that never moved.
  // ===========================================================================
  const deviceCalls = page.requests.filter((r) => DEVICE.test(new URL(r.url).pathname));
  // A press that moves the front end lands on `/center` (this centre, keep the rate) or, since
  // T-529, on `/window` (this whole capture configuration, one action). Both are "the radio moved",
  // which is the only thing this control is claiming.
  const centerCalls = deviceCalls.filter((r) => /\/(center|window)$/.test(new URL(r.url).pathname));
  t.diagnostic(`device route calls over the whole run: ${deviceCalls.length} (${centerCalls.length} centre changes)`);
  assert.ok(centerCalls.length >= 2,
    `expected at least 2 centre changes (away to B, back to A) from the explicit retune presses; got ${centerCalls.length}: ` +
    `${deviceCalls.map((r) => new URL(r.url).pathname).join(", ")}`);

  assert.deepEqual(page.exceptions, [], "an uncaught exception occurred during the journey");

  await page.shot(path.join(ART, "fog-of-war.png"));
});
