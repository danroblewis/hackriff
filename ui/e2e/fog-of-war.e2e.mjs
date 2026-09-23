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
import { Browser, census, waitWhileWorking } from "./harness.mjs";
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

/**
 * `GET` against the backend, **retrying its backpressure** — the same shape as
 * `canvas-journey.e2e.mjs`'s and `scan-everything.e2e.mjs`'s `get` (T-690).
 *
 * `/api/tiles` answers `503` over `cost.in_flight_limit` concurrent reads and takes its slot
 * before it does any work, and this file's own browser holds up to four of them — so a bare
 * `fetch` here manufactures the refusal and then reads it as an answer. A `503` is "busy now",
 * never "no". `/api/coverage` shares the history lock behind it. Every premise this file waits on
 * goes through here, so one unretried refusal ended the whole journey.
 */
async function get(backend, p, { tries = 40, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${p}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    assert.ok(r.status === 503 && i < tries,
      `GET ${p} -> ${r.status}${r.status === 503 ? ` after ${i} retries of the route's backpressure` : ""}`);
    await new Promise((res) => setTimeout(res, waitMs));
  }
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

/**
 * Wait for the pane to settle on an answer: tiles in hand, nothing outstanding, no stand-ins.
 *
 * `surveyMayAnswer` is T-580's case and is opt-in **per call site**, never global. Over spectrum
 * the coverage survey settles as never sampled the surface requests no tile at all, so a band-C
 * pane reaches its final state at `0 tiles` and this would otherwise wait out its whole timeout.
 * But the relaxation must not travel to the bands that WERE swept: there, `0 tiles` with the survey
 * momentarily answering is a pane that has not loaded yet, and returning on it hands the caller a
 * half-drawn frame to measure brightness in — which is exactly how phase 2's live baseline came out
 * dimmer than the shadow it is the baseline for.
 *
 * **Bounded by whether the pane is still WORKING, not by 20 s** (the deflake, 2026-09-22). A fixed
 * deadline here is the same bet `draw()`'s note above already refuses, one layer down: the tile
 * route's service rate moves more than twenty-fold between a quiet box and the gate's pooled lanes,
 * so on a busy box this returned `resident: false` on a pane that was simply mid-fill and the
 * caller then measured a half-drawn frame. `waitWhileWorking` keeps waiting while the pane's own
 * report changes or its requests are on the wire, and gives up when both have been quiet — which is
 * the state "it will not converge" actually looks like.
 */
async function waitForResident(page, { timeoutMs = 120000, everyMs = 400, stallMs = 10000, surveyMayAnswer = false } = {}) {
  const r = await waitWhileWorking(page, async () => (await pane0(page)).counts, (counts) => {
    const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(counts);
    // `· N never sampled` is the pane saying the survey answered the place — see `draw()`.
    const surveyed = surveyMayAnswer && /· (\d+) never sampled/.test(counts);
    return !!m && (Number(m[1]) > 0 || surveyed) && Number(m[2]) === 0 && Number(m[3]) === 0;
  }, { everyMs, stallMs, timeoutMs });
  return { resident: r.ok, counts: r.value, ms: r.ms, stalledMs: r.stalledMs };
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
/**
 * The tile answers one replayed request carries.
 *
 * **T-573/T-700 made a viewport's tiles arrive in ONE request** (`GET /api/tiles/batch`), whose
 * body is `{ tiles: [{ address, status, tile }, …] }` — each `tile` being exactly a single-tile
 * answer. A replay that only understood the single-tile body read every batch as "carried no usable
 * grid" and then reported the absence as "no pane tile response covers band A", which is how this
 * file went red the moment batching landed. Both shapes are the pane's own answers, so both are
 * unwrapped here, in one place, and each entry keeps the `address.spelling` it came with.
 */
function tileAnswersOf(json) {
  if (Array.isArray(json?.tiles)) {
    return json.tiles
      .filter((e) => e?.tile)
      .map((e) => ({ tile: e.tile, spelling: e.address?.spelling ?? null }));
  }
  return json ? [{ tile: json, spelling: null }] : [];
}

/**
 * Replay one request URL, riding out the route's 503s. `null` when it never answered.
 *
 * **The batch route refuses PER ENTRY** (T-573: "a 503 rejects only its own address … the tiles
 * beside it resolve"), so a 200 whose every entry is a refusal is the same "ask again" a bare 503
 * is — and reading it as an answer is exactly the T-690 defect one level down: it was reported as
 * "38 requests replayed, 0 tile answers, 0 covered other spectrum", which is a busy route wearing
 * the words of a band that was never drawn. An all-refused batch is therefore retried on the same
 * capped cadence as the status code itself.
 */
async function replay(url, backend) {
  for (let attempt = 0; attempt < 12; attempt++) {
    let resp = null;
    try {
      resp = await fetch(url, { headers: { authorization: `Bearer ${backend.token}` } });
    } catch { return null; }
    if (resp.status !== 503) {
      if (!resp.ok) return null;
      let json = null;
      try { json = await resp.json(); } catch { return null; }
      const allRefused = Array.isArray(json?.tiles) && json.tiles.length > 0
        && json.tiles.every((e) => !e?.tile);
      if (!allRefused) return json;
    }
    await new Promise((r) => setTimeout(r, Math.min(1000, 100 * 2 ** attempt)));
  }
  return null;
}

/** Every `/api/tiles` request the page made — single or batch, never the events route. */
const tileRequests = (page, sinceIdx) => page.requests.slice(sinceIdx)
  .filter((r) => r.url.includes("/api/tiles") && !r.url.includes("/api/tiles/events"));

async function findPaneTileFor(page, backend, targetHz, { sinceIdx = 0, maxSpanHz = 80e6 } = {}) {
  const reqs = tileRequests(page, sinceIdx);
  // Counted in ANSWERS, not requests: since T-573/T-700 one batch request carries many tiles, so
  // "18 covered other spectrum" out of "15 requests" would otherwise read as an arithmetic error
  // in the very message someone reads when this goes red.
  const why = { answers: 0, refused: 0, failed: 0, shapeless: 0, tooWide: 0, elsewhere: 0 };
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
    json = await replay(reqs[i].url, backend);
    if (!json) { why.refused++; continue; }
    // A batch answer carries many tiles; the one that drew `targetHz` is whichever covers it.
    let hit = null;
    for (const { tile } of tileAnswersOf(json)) {
      why.answers++;
      const g = tile?.grid;
      if (!g || !(g.nf > 0) || !(g.f_cell_hz > 0)) { why.shapeless++; continue; }
      const spanHz = g.nf * g.f_cell_hz;
      if (spanHz > maxSpanHz) { why.tooWide++; continue; }
      if (targetHz < g.f_lo_hz || targetHz >= g.f_lo_hz + spanHz) { why.elsewhere++; continue; }
      hit = { url: reqs[i].url, json: tile, spanHz };
      break;
    }
    if (hit) return hit;
  }
  return { none: true, tried: reqs.length, why };
}

/** `findPaneTileFor`'s answer, or a failure that says WHICH reason it ran out of. */
function tileOrWhy(found, label) {
  assert.ok(found && !found.none,
    `${label}. Of ${found?.tried ?? 0} pane tile request(s) replayed, ` +
    `${found?.why?.refused ?? 0} the route would not answer even after retrying its 503; of the ` +
    `${found?.why?.answers ?? 0} tile answer(s) they carried, ${found?.why?.shapeless ?? 0} had no ` +
    `usable grid, ${found?.why?.tooWide ?? 0} were wider than a pane's and ` +
    `${found?.why?.elsewhere ?? 0} covered other spectrum. A route that was busy and a band that ` +
    "was never drawn are different findings.");
  return found;
}

/**
 * Poll until the PANE'S OWN tile answer reads band `targetHz` as mostly observed at the live edge
 * — the exact quantity the baseline assertion below reads, from the exact source it reads it from.
 *
 * `waitForObservedSince` is the same rule over `/api/coverage` and it is not a substitute here: it
 * asks a 16 x 16 grid over +/-1 MHz since a chosen instant, while the assertion reads ~26 rows of
 * ONE column of a tile's own coverage plane ending at that tile's fold horizon. Those are different
 * windows over the same map, so 0.5 in one does not mean 0.5 in the other — measured: the coverage
 * query satisfied at 224/256 while the tile plane still read 13/26. It also covers the case where
 * the pane has not finished fetching band A at all, because it keeps looking as new requests land.
 *
 * Reports rather than throws: a radio that genuinely never observed the band reaches the caller's
 * assertion and fails there with the measured number, as everything else in this file does.
 */
async function waitForPaneTileObserved(page, backend, targetHz, { want = 0.5, timeoutMs = 45000, everyMs = 500 } = {}) {
  const t0 = Date.now();
  let last = null;
  for (;;) {
    const found = await findPaneTileFor(page, backend, targetHz, { sinceIdx: 0 });
    if (!found.none) {
      const cov = decodeCoveragePlane(found.json.coverage);
      const nr = edgeRowsCoverage(cov, targetHz, found.json.shadow.edge_s);
      last = { found, nr };
      if (nr.observedShare > want) return { ...last, ms: Date.now() - t0, reached: true };
    } else if (!last) {
      last = { found, nr: null };
    }
    if (Date.now() - t0 > timeoutMs) return { ...last, ms: Date.now() - t0, reached: false };
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/**
 * Replay the most recent `GET /api/coverage` request the PANE itself made whose answer covers
 * `targetHz` — T-580's survey, the request that decided the grey on screen.
 *
 * This is the same technique as [[findPaneTileFor]] pointed at the other route, and since T-580 it
 * is the ONLY one that works over never-swept spectrum: the surface asks the coverage map first and
 * does not request a tile whose whole span the survey settles as never sampled, so a band the radio
 * never visited has a survey answer behind its pixels and no tile answer at all. Its four-state
 * `any.cells` vocabulary is the same one `/api/tiles`' `coverage` plane uses, from the same
 * record-derived computation (docs/api.md), so the claim being checked is unchanged.
 */
async function findPaneSurveyFor(page, backend, targetHz, { sinceIdx = 0 } = {}) {
  const reqs = page.requests.slice(sinceIdx).filter((r) => r.url.includes("/api/coverage"));
  const why = { refused: 0, shapeless: 0, elsewhere: 0 };
  for (let i = reqs.length - 1; i >= 0; i--) {
    let json = null;
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
    const g = json.grid, cells = json.any?.cells;
    if (!g || !(g.cells > 0) || !(g.rows > 0) || !(g.f_cell_hz > 0) || !Array.isArray(cells)
      || cells.length !== g.cells * g.rows) { why.shapeless++; continue; }
    const spanHz = g.cells * g.f_cell_hz;
    if (targetHz < g.f_lo_hz || targetHz >= g.f_lo_hz + spanHz) { why.elsewhere++; continue; }
    return { url: reqs[i].url, json, spanHz };
  }
  return { none: true, tried: reqs.length, why };
}

/** How the survey reads for `targetHz` across every row it holds: the four-state answer the pane's
 * own grey came out of. `rows` is small (T-580 asks for two), so all of them are reported. */
function surveyColumn(json, targetHz) {
  const g = json.grid, cells = json.any.cells;
  const col = Math.min(g.cells - 1, Math.max(0, Math.floor((targetHz - g.f_lo_hz) / g.f_cell_hz)));
  const states = [];
  for (let r = 0; r < g.rows; r++) states.push(cells[r * g.cells + col]?.state ?? null);
  const unobserved = states.filter((s) => s === "unobserved").length;
  return { col, rows: g.rows, states, unobserved, unobservedShare: g.rows ? unobserved / g.rows : 0 };
}

/**
 * A tile answer over `targetHz`, addressed by **stepping the pane's own most recent tile URL**
 * across to it — the test's own probe, and it says so.
 *
 * It exists for one claim: the shadow plane lives on `/api/tiles` alone, and since T-580 the
 * product deliberately never asks for a tile over never-swept spectrum, so the anti-case "no shadow
 * ever bleeds onto spectrum this run's radio never looked at" has no pane request left to replay.
 * Rather than drop the guard, this addresses the tile — but it still does **not** re-implement
 * `ui/src/surface/lattice.ts`' addressing (the T-397 trap this file's header names): it takes a
 * real pane URL, reads the tile width out of the SERVER's own answer (`grid.nf * grid.f_cell_hz`),
 * steps `f_index` by whole tiles of that width, and then checks the answer that comes back really
 * does cover `targetHz` before anything is read off it. Level, scheme, cells and time index are the
 * pane's own, untouched.
 */
async function tileSteppedTo(page, backend, targetHz, { sinceIdx = 0, maxSpanHz = 80e6 } = {}) {
  const from = await findPaneTileFor(page, backend, targetHz, { sinceIdx, maxSpanHz })
    .then((f) => (f.none ? null : f));
  if (from) return from;
  // The pane is at band C, so its own requests are elsewhere; take the newest usable one whatever
  // spectrum it covers, single-tile or batch (T-573/T-700).
  const reqs = tileRequests(page, sinceIdx);
  for (let i = reqs.length - 1; i >= 0; i--) {
    const seed = new URL(reqs[i].url, backend.origin);
    const batch = seed.pathname.endsWith("/batch");
    const json = await replay(reqs[i].url, backend);
    if (!json) continue;
    for (const { tile, spelling } of tileAnswersOf(json)) {
      const g = tile?.grid;
      if (!g || !(g.nf > 0) || !(g.f_cell_hz > 0)) continue;
      const spanHz = g.nf * g.f_cell_hz;
      if (spanHz > maxSpanHz) continue;
      const steps = Math.floor((targetHz - g.f_lo_hz) / spanHz);
      if (!Number.isFinite(steps)) continue;
      // Step the frequency index of the address this very answer came back for, and leave every
      // other field of the pane's own request alone. On the batch route the address is the
      // `level_f.level_t.f_index.t_index` spelling the answer itself quotes, so the one that is
      // stepped is never guessed; on the single route it is the URL's `f_index`.
      const stepped = new URL(seed.toString());
      if (batch) {
        if (!spelling) continue;
        const parts = spelling.split(".");
        if (parts.length !== 4) continue;
        const fIdx = Number(parts[2]);
        if (!Number.isFinite(fIdx)) continue;
        parts[2] = String(fIdx + steps);
        stepped.searchParams.set("addresses", parts.join("."));
      } else {
        const idx = Number(seed.searchParams.get("f_index"));
        if (!Number.isFinite(idx)) continue;
        stepped.searchParams.set("f_index", String(idx + steps));
      }
      const answer = await replay(stepped.toString(), backend);
      for (const { tile: st } of tileAnswersOf(answer)) {
        const sg = st?.grid;
        if (!sg || !(sg.nf > 0) || !(sg.f_cell_hz > 0)) continue;
        const sSpan = sg.nf * sg.f_cell_hz;
        if (targetHz < sg.f_lo_hz || targetHz >= sg.f_lo_hz + sSpan) continue;
        return { url: stepped.toString(), json: st, spanHz: sSpan, stepped: steps };
      }
    }
  }
  return { none: true, tried: reqs.length };
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
 * Split a pane's ROI into **the part the surface says it drew** and the strip above it.
 *
 * T-580's fourth rule: a skipped tile is drawn grey only as far forward as the survey's own
 * evidence reaches (`horizon.as_of_s`), and the rows above that are left as the pane's PENDING
 * ground — *"not known yet"*, which is true, and which the pane states in words as `drawn to X s
 * short of the top`. A pane following the live edge therefore always carries such a strip, at the
 * top (newest time), about as tall as the survey's own re-ask cadence; over a window a few seconds
 * wide that is a large share of the screen, and measuring "is band C grey?" across the whole ROI
 * measures the strip instead of the claim.
 *
 * The split is found in the PIXELS rather than computed from the readout: scan rows downward for
 * the first that is mostly grey. Both halves are then measured and asserted separately — the drawn
 * half for the grey claim, the strip for carrying no coverage claim at all — and the readout is
 * checked against the split rather than trusted for it.
 */
/**
 * Split a pane's ROI into **the part the surface says it drew** and the ground above it.
 *
 * Every brightness claim in this file is a comparison between two panes, and a pane following the
 * live edge does not reach its own window top: the newest rows are ones it has no answer for yet,
 * and they are left as the pane's ground — which it states in words, `drawn to X s short of the
 * top`. How far short varies with how busy the box is, from 0.1 s to 1.3 s over a window a few
 * seconds wide, so measuring a mean brightness across the whole ROI measures HOW FAR THE PANE GOT
 * as much as it measures the pixels. Observed exactly that way: a live baseline at meanLuma 54
 * against a shadow of 54.7, failing as "the shadow ceiling is not visibly in effect" while the
 * ceiling was in effect and the baseline was half ground.
 *
 * T-580 makes the same strip appear over never-swept spectrum for its own reason (rule 4: grey
 * reaches only as far forward as the survey's evidence does), so one split serves both: measure the
 * drawn part for every claim, and assert the strip separately for what it must NOT contain.
 *
 * The split is found in the PIXELS, not computed from the readout, so the readout can be checked
 * against it rather than trusted for it.
 */
function splitAtDrawnTop(img, rect, notGround = null, tol = 2) {
  const x0 = Math.max(0, Math.round(rect.x)), y0 = Math.max(0, Math.round(rect.y));
  const x1 = Math.min(img.width, Math.round(rect.x + rect.w)), y1 = Math.min(img.height, Math.round(rect.y + rect.h));
  const full = { strip: { x: x0, y: y0, w: x1 - x0, h: 0 }, drawn: { x: x0, y: y0, w: x1 - x0, h: Math.max(0, y1 - y0) }, stripShare: 0 };
  if (!(x1 > x0) || !(y1 > y0)) return full;
  // The ground is whatever the pane's TOP row is made of, and only when that row is one flat
  // colour: a row of real measurement is not (a live pane's own rows run to ~1000 distinct
  // colours). A pane drawn all the way to its top therefore splits at zero, by construction.
  const share = (y, rgb) => {
    let hit = 0;
    for (let x = x0; x < x1; x++) {
      const d = (y * img.width + x) * 4;
      if (Math.abs(img.data[d] - rgb[0]) <= tol && Math.abs(img.data[d + 1] - rgb[1]) <= tol
        && Math.abs(img.data[d + 2] - rgb[2]) <= tol) hit++;
    }
    return hit / (x1 - x0);
  };
  const t = (y0 * img.width + x0) * 4;
  const ground = [img.data[t], img.data[t + 1], img.data[t + 2]];
  if (share(y0, ground) < 0.9) return full;
  // **THE grey is a drawn answer, not the ground.** A band the survey settles as never sampled is
  // drawn flat grey all the way up, so without this the detector reads its own subject as ground
  // and reports a pane that drew perfectly as having drawn nothing ("band C drew NO grey at all").
  // `notGround` is the one colour the caller knows is a measurement claim rather than bare canvas.
  if (notGround && Math.abs(ground[0] - notGround[0]) <= tol && Math.abs(ground[1] - notGround[1]) <= tol
    && Math.abs(ground[2] - notGround[2]) <= tol) return full;
  let split = y1;
  for (let y = y0; y < y1; y++) { if (share(y, ground) < 0.5) { split = y; break; } }
  return {
    strip: { x: x0, y: y0, w: x1 - x0, h: Math.max(0, split - y0) },
    drawn: { x: x0, y: split, w: x1 - x0, h: Math.max(0, y1 - split) },
    stripShare: (split - y0) / (y1 - y0),
    ground,
  };
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
 *
 * **The rectangle comes back WITH the pixels** (the deflake, 2026-09-22). Every call site used to
 * read the pane's box with `paneGeometry` *before* this function ran and measure the screenshot in
 * it afterwards — and this function can sit for twenty-five seconds, during which the chrome's own
 * height is not constant: T-505 put the tier inside every viewport row's level cell, so that row
 * wraps and un-wraps as the level resolves and the canvas moves with it. A stale rectangle samples
 * the page AROUND the pane, which is one flat colour, so `splitAtDrawnTop` reads the whole ROI as
 * ground and hands back a zero-height "drawn" part — a census of **0 distinct colours**, which is
 * exactly the shape this file was quarantined for ("band A at boot is not a real render (only 0
 * distinct colours)") while the pane itself reported tiles drawn. `canvas-journey.e2e.mjs` carries
 * the same note against the same mistake. So the box is read, the shot taken, and the box read
 * again; if it moved, the pair is taken again rather than trusted.
 *
 * `needsRender` adds the second half of that boot wait: keep re-snapping until the pane has
 * actually **presented a frame into its own rectangle** — a `drawn` part with height, which is
 * strictly weaker than any claim the callers make about it, so their assertions still judge the
 * sample rather than being satisfied by the wait.
 */
async function draw(page, { settleMs = 600, timeoutMs = 25000, needsRender = false, notGround = null } = {}) {
  await page.frames(4);
  const counts = `(document.querySelector('${PANE_ROW} .hk-surface-counts')?.textContent ?? '')`;
  // **A pane over never-swept spectrum holds no tiles and never will** (T-580). The original
  // predicate here was `0 pending` AND at least one tile, because until T-580 every place on
  // screen was a tile and `0 tiles` could only mean the pane had not started. Since T-580 the
  // surface asks `GET /api/coverage` first and *never requests* a tile whose whole span the survey
  // settles as never sampled: such a pane is fully drawn while reporting `0 tiles · 0 coarse
  // stand-ins · 0 pending`, so the old predicate could never fire on it and every band-C
  // measurement below was taken at the 25 s timeout instead of at a settled frame. The pane says
  // which of the two it is — `· N never sampled` — so that is what is waited on: nothing
  // outstanding, and something actually on the screen, tiles or survey.
  const drew = await page.waitFor("the pane to report itself drawn (0 pending, and drawing something)",
    `/· 0 pending/.test(${counts}) && (!/^0 tiles/.test(${counts}) || /never sampled/.test(${counts}))`,
    { timeoutMs })
    .then(() => true, () => false);
  // A short settle after the page says it is drawn: the last upload and the frame that uses it are
  // not the same tick. A floor, not a budget — it is not waiting for the route.
  await new Promise((r) => setTimeout(r, settleMs));
  await page.frames(4);
  let snap = await snapPane(page);
  if (needsRender) {
    const t0 = Date.now();
    while (splitAtDrawnTop(snap.img, snap.roi, notGround).drawn.h <= 0 && Date.now() - t0 < timeoutMs) {
      await page.frames(6);
      snap = await snapPane(page);
    }
    snap.renderWaitMs = Date.now() - t0;
  }
  const img = snap.img;
  img.drew = drew;
  img.roi = snap.roi;
  img.pane = snap.g.pane;
  img.movedWhileShooting = snap.moved;
  img.renderWaitMs = snap.renderWaitMs ?? 0;
  img.counts = await page.eval(counts);
  return img;
}

/**
 * A screenshot and the pane rectangle it is measured in, read as one thing.
 *
 * The box is read either side of the shot and the pair retaken if it moved, so the pixels and the
 * coordinates they are indexed by come from the same layout. See [[draw]] for what a stale one costs.
 */
async function snapPane(page, { tries = 4 } = {}) {
  let before = await paneGeometry(page), img = null, after = before;
  for (let i = 0; i <= tries; i++) {
    img = await page.shot();
    after = await paneGeometry(page);
    const same = ["x", "y", "w", "h"].every((k) => before.rect[k] === after.rect[k]);
    if (same) return { img, g: after, roi: roiOf(after.pane), moved: false };
    before = after;
  }
  return { img, g: after, roi: roiOf(after.pane), moved: true };
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
  const sinceOpen = Date.now() / 1000;
  const gA0 = await gotoFreq(page, at, A_HZ, A_VIEW_SPAN_HZ);
  t.diagnostic(`opened onto band A: ${spanOf(gA0.view)} (${gA0.trail.length} nav step(s))`);
  assertNoDeviceCalls(page, openIdx, "panning onto band A at boot");
  // **Wait for the QUANTITY the baseline is measured in, from the server** — the same rule (T-690)
  // and the same helper phase 5 already uses before ITS brightness comparison. `edgeRowsCoverage`
  // below reads a window of rows ending at the tile's own fold horizon and needs band A to be
  // mostly *observed* across it, and the pixel baseline needs the pane to hold a real render; both
  // are statements about how much the mock SDR has put in the coverage map, and neither is a
  // statement about elapsed time. Without it this phase measured whatever had accumulated by the
  // time `waitForRecordToCover` returned, which on a loaded box was as little as 42 % observed and
  // a one-colour pane — and it failed as "band A at boot is not a real render", which reads as a
  // product defect and is not one. It reports rather than throws, so a mock that genuinely never
  // observed band A still reaches the assertions below and fails there with the number.
  const live0 = await waitForObservedSince(backend, A_HZ, sinceOpen, { want: 0.5 });
  t.diagnostic(`band A observed by the server before the baseline, after ${live0.ms} ms: ` +
    `${live0.observed}/${live0.known} known cells` +
    (live0.reached ? "" : " — NEVER REACHED the premise, measuring anyway"));
  const wr0 = await waitForRecordToCover(page, backend);
  t.diagnostic(`record covers the pane after ${wr0.ms} ms (age ${wr0.ageS?.toFixed?.(1) ?? "?"} s, pane span bound ${wr0.paneS?.toFixed?.(1) ?? "?"} s, ruler "${wr0.ruler}")${wr0.timedOut ? " — TIMED OUT, measuring anyway" : ""}`);
  // **The pane's wait comes AFTER the server's, not before it.** It used to run the moment the view
  // reached band A — before the mock had put anything there — so it settled on whatever was in hand
  // (often one tile that drew nothing), and `draw()`'s `0 pending` was then satisfied by a pane with
  // nothing on it. The baseline was measured there and the run failed as "band A at boot is not a
  // real render", which reads as a product defect and is not one: the pane cannot hold data the
  // server does not have yet.
  const res0 = await waitForResident(page);
  t.diagnostic(`pane resident after ${res0.ms} ms: "${res0.counts}"${res0.resident ? "" : " — NEVER became resident, measuring anyway"}`);
  // **`needsRender`, and the rectangle read with the pixels.** This is the phase the file was
  // quarantined on: "band A at boot is not a real render (only 0 distinct colours)" at ~8 s, with
  // the pane itself reporting tiles drawn. Zero distinct colours is not a flat pane — it is a
  // `drawn` part of zero height, which is what `splitAtDrawnTop` returns when the rectangle handed
  // to it is not on the pane any more, or when the pane has not yet presented a frame into it. Both
  // are readiness, and both are now waited out on the page's own terms (see [[draw]]); the two
  // assertions below are untouched and still judge the frame that comes back.
  const imgA0 = await draw(page, { needsRender: true, notGround: marks.greyRgb });
  const roiA = imgA0.roi;
  t.diagnostic(`band A's first frame landed in the pane's own rectangle after ${imgA0.renderWaitMs} ms` +
    (imgA0.movedWhileShooting ? " — the canvas was still moving when it was shot" : ""));
  // Measured over the part the pane SAYS it drew, never across its ground: the strip a
  // following pane leaves at its top varies with load, and a mean brightness taken across it
  // compares how far two panes got as much as it compares their pixels (`splitAtDrawnTop`).
  const cutA0 = splitAtDrawnTop(imgA0, roiA, marks.greyRgb);
  const liveA = inspect(imgA0, cutA0.drawn, marks.greyRgb, marks.inkRgb);
  t.diagnostic(`LIVE A baseline: meanLuma ${liveA.census.meanLuma.toFixed(1)}, grey ${(liveA.greyShare * 100).toFixed(1)}%, ` +
    `ink ${(liveA.inkShare * 100).toFixed(1)}%, distinct ${liveA.census.distinct} · drawn with "${imgA0.counts}"` +
    (imgA0.drew ? "" : " — THE PANE NEVER REPORTED ITSELF DRAWN"));
  assert.ok(liveA.census.distinct >= 4, `band A at boot is not a real render (only ${liveA.census.distinct} distinct colours) — this run proves nothing`);
  assert.ok(liveA.greyShare < 0.5, `band A at boot already reads mostly grey (${(liveA.greyShare * 100).toFixed(1)}%) — this run has no live baseline to lose`);

  const baseA = await waitForPaneTileObserved(page, backend, A_HZ);
  const tileA0 = tileOrWhy(baseA.found,
    "no pane tile response covers band A while it is live — cannot establish the server-side baseline");
  {
    const cov = decodeCoveragePlane(tileA0.json.coverage);
    const nr = baseA.nr ?? edgeRowsCoverage(cov, A_HZ, tileA0.json.shadow.edge_s);
    if (!baseA.reached) {
      t.diagnostic(`band A never reached the observed premise in ${baseA.ms} ms — asserting on what it did reach`);
    }
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
  const imgA1 = await draw(page, { needsRender: true, notGround: marks.greyRgb });
  // Measured over the part the pane SAYS it drew, never across its ground: the strip a
  // following pane leaves at its top varies with load, and a mean brightness taken across it
  // compares how far two panes got as much as it compares their pixels (`splitAtDrawnTop`).
  const cutA1 = splitAtDrawnTop(imgA1, imgA1.roi, marks.greyRgb);
  const shadowA = inspect(imgA1, cutA1.drawn, marks.greyRgb, marks.inkRgb);
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
  // The one call site where T-580's short-circuit is the expected answer: this pane's whole window
  // is spectrum the radio never visited, so `0 tiles` here is the product working, not a pane that
  // has not loaded.
  await waitForResident(page, { surveyMayAnswer: true });
  // NOT `needsRender`: a band the survey settles as never sampled is drawn flat grey all the way
  // up, which `splitAtDrawnTop` reports as a zero-height strip and a full-height drawn part only
  // because `notGround` tells it THE grey is an answer. Waiting for a "drawn" part here would be
  // waiting for the one thing this phase is asserting is absent. The rectangle still comes back
  // with the pixels, which is the half of the fix that applies everywhere.
  const imgC = await draw(page);
  const greyC = inspect(imgC, imgC.roi, marks.greyRgb, marks.inkRgb);
  t.diagnostic(`GREY C (never swept): meanLuma ${greyC.census.meanLuma.toFixed(1)}, grey ${(greyC.greyShare * 100).toFixed(1)}%, ` +
    `ink ${(greyC.inkShare * 100).toFixed(1)}%, distinct ${greyC.census.distinct} · drawn with "${imgC.counts}"` +
    (imgC.drew ? "" : " — THE PANE NEVER REPORTED ITSELF DRAWN"));

  // **T-580, positively: the pane asked for NO TILE over band C at all.** Since the surface
  // consults `GET /api/coverage` before it requests a tile, and skips any tile whose whole span the
  // survey settles as never sampled, an absence of pane tile requests here is the *product working*
  // rather than a band that failed to draw — and it is a claim worth pinning, because it is the
  // whole point of the short-circuit: never-swept spectrum costs no round trip. Asserted from the
  // same replay the phases above use, so the two readings cannot drift apart; `elsewhere` is
  // exactly "the pane did request tiles, and none of them covered band C".
  const askedC = await findPaneTileFor(page, backend, C_HZ, { sinceIdx: sinceCIdx });
  t.diagnostic(`pane tile requests since panning to band C: ${askedC.none ? askedC.tried : "≥1 covering C"}` +
    (askedC.none ? `, carrying ${askedC.why.answers} tile answer(s), of which ${askedC.why.elsewhere} covered other spectrum` : ""));
  assert.ok(askedC.none,
    "the pane REQUESTED a tile over band C, spectrum the coverage survey settles as never sampled " +
    "— T-580's short-circuit did not fire, and every grey pixel here cost a round trip");

  // THE CLAIM, server-side, from the request the pane DID make. The survey is `/api/coverage`'s
  // four-state answer over the same record-derived map the tile route's `coverage` plane comes from
  // (docs/api.md), so this is the identical claim read off the identical computation — just off the
  // route the product now uses for it.
  const survC = await findPaneSurveyFor(page, backend, C_HZ, { sinceIdx: 0 });
  assert.ok(!survC.none,
    `no pane coverage-survey response covers band C — cannot check it against the server. Of ` +
    `${survC.tried} survey request(s) replayed: ${survC.why?.refused ?? 0} the route would not ` +
    `answer, ${survC.why?.shapeless ?? 0} carried no usable grid, ${survC.why?.elsewhere ?? 0} ` +
    "covered other spectrum.");
  const colC = surveyColumn(survC.json, C_HZ);
  t.diagnostic(`SERVER band C (survey): column ${colC.col}, ${colC.rows} row(s) read ` +
    `[${colC.states.join(", ")}] — unobserved ${colC.unobserved}/${colC.rows}`);
  assert.ok(colC.unobservedShare > 0.9,
    `the survey does not settle band C as unobserved, so the pane's grey there is not the ` +
    `"never looked" claim: ${JSON.stringify(colC)}`);

  // THE ANTI-CASE, server-side: THE SHADOW PLANE IS EMPTY over band C's whole tile — not merely
  // absent at one column. The guard against a shadow bleeding onto spectrum this run's radio never
  // looked at. The shadow plane lives on `/api/tiles` alone and the product no longer asks for that
  // tile (the assertion above is that it must not), so this one is addressed by the test itself,
  // by stepping a real pane URL across — see `tileSteppedTo` for why that is not a second copy of
  // the client's addressing.
  const tileC = await tileSteppedTo(page, backend, C_HZ, { sinceIdx: sinceCIdx });
  assert.ok(!tileC.none,
    `could not reach a tile over band C by stepping any of ${tileC.tried} pane tile URL(s) across ` +
    "to it — the shadow anti-case has nothing to read");
  t.diagnostic(`SERVER band C (tile, stepped ${tileC.stepped ?? 0} tile(s) across from a pane's own ` +
    `URL): whole-tile shadow.runs = ${tileC.json.shadow.runs}`);
  assert.equal(tileC.json.shadow.runs, 0,
    `band C — never tuned to in this run — carries a SHADOW RUN. A shadow appeared over spectrum ` +
    `the radio never looked at: ${JSON.stringify(tileC.json.shadow)}`);

  // THE ANTI-CASE, pixel-side: reads as flat grey, and carries none of the ink — measured over the
  // part the surface says it drew. See `splitAtDrawnTop` for why the two halves are measured apart:
  // a following pane always carries the survey's `as_of` strip at the top, and it is the pane's
  // honest "not known yet" ground, not a coverage claim.
  const cut = splitAtDrawnTop(imgC, imgC.roi, marks.greyRgb);
  const drawnC = inspect(imgC, cut.drawn, marks.greyRgb, marks.inkRgb);
  const stripC = cut.strip.h > 0 ? inspect(imgC, cut.strip, marks.greyRgb, marks.inkRgb) : null;
  t.diagnostic(`GREY C split: drawn ${cut.drawn.h}px grey ${(drawnC.greyShare * 100).toFixed(1)}% ` +
    `ink ${(drawnC.inkShare * 100).toFixed(1)}% distinct ${drawnC.census.distinct} · ` +
    `survey-horizon strip ${cut.strip.h}px (${(cut.stripShare * 100).toFixed(1)}% of the pane)` +
    (stripC ? `, grey ${(stripC.greyShare * 100).toFixed(1)}% ink ${(stripC.inkShare * 100).toFixed(1)}%` : ""));

  assert.ok(cut.drawn.h > 0, `band C drew NO grey at all: the whole pane is the survey-horizon strip — ${imgC.counts}`);
  assert.ok(drawnC.greyShare > 0.9,
    `band C (never swept) does not read as mostly grey where the surface says it drew ` +
    `(${(drawnC.greyShare * 100).toFixed(1)}%): ${JSON.stringify(drawnC.census)}`);
  assert.ok(greyC.inkShare < 0.02,
    `band C (never swept) shows the shadow's ink pattern (${(greyC.inkShare * 100).toFixed(1)}%) — a shadow drawn where nothing was ever observed`);
  // The strip is allowed, and it is NOT allowed to be a coverage claim: it carries neither the
  // grey (which would say "never looked", a claim the survey has not reached yet) nor the ink.
  if (stripC) {
    assert.ok(stripC.greyShare < 0.1 && stripC.inkShare < 0.02,
      `the strip above the survey's horizon is drawn as a coverage answer (grey ` +
      `${(stripC.greyShare * 100).toFixed(1)}%, ink ${(stripC.inkShare * 100).toFixed(1)}%) — ` +
      "it is the pane's not-known-yet ground, and grey there would claim the radio never looked " +
      "over an interval the survey has not spoken about");
    // ...and the pane must SAY it, rather than leave it to be discovered in the pixels.
    assert.match(imgC.counts, /drawn to [\d.]+ s short of the top/,
      `the pane left ${cut.strip.h}px undrawn at the top and its readout does not say so: "${imgC.counts}"`);
  }
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
  const imgA2 = await draw(page, { needsRender: true, notGround: marks.greyRgb });
  // Measured over the part the pane SAYS it drew, never across its ground: the strip a
  // following pane leaves at its top varies with load, and a mean brightness taken across it
  // compares how far two panes got as much as it compares their pixels (`splitAtDrawnTop`).
  const cutA2 = splitAtDrawnTop(imgA2, imgA2.roi, marks.greyRgb);
  const reswptA = inspect(imgA2, cutA2.drawn, marks.greyRgb, marks.inkRgb);
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
