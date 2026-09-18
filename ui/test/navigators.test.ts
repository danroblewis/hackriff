// **The navigation ARITHMETIC that outlived the two bespoke edge scrubbers** (T-445 trimmed this
// file; T-340 wrote it).
//
// The widgets are gone: `ui/src/app/centre/navigators.ts`, 1 528 lines of bar geometry, hover
// readouts, drag-to-zoom, wheel-pan and two mounts, retired by docs/16 §8.5 into canvas pan/zoom
// plus the minimap. What survives is `ui/src/navigators.ts` — the **pure** module that turns
// backend answers into an axis — and it survives because the unified surface reads exactly the same
// answers through exactly the same functions:
//
//  - `activeWindows` / `litSegments`: the reported capture windows, now the minimap's lit segments
//    (`surface/minimap.ts` re-exports `activeWindows` and `surface-minimap.test.ts` asserts it is
//    the *same object*, so there is no second reader to disagree);
//  - `spectrumExtent`: the device-available range, now the surface's own frequency bounds
//    (`surface/bootstrap.ts`);
//  - `surveyCells` / `valueAt` / `unobservedCount`: unobserved is not quiet, which is the coverage
//    rule the whole canvas is built on;
//  - `stripCells`: one cell per pixel, never a constant — the T-411/T-397 resolution rule;
//  - `defaultViewport` / `surveyViewport`: open on the tune, not on the middle of 6 GHz.
//
// The tests below are the ones that assert those properties. The ones that asserted a *widget* —
// its drag threshold, its readout string, its wheel direction, its CSS scoping — went with the
// widget, and the user-visible invariants behind them are re-pointed at the canvas in
// `ui/test/surface-cutover.test.ts`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import * as ax from "../src/axis";
import { snapState, type NavigationGrid } from "../src/navigation";
import {
  DRAG_PX, TOUCH_DRAG_PX, TOUCH_GRAB_PX, activeWindows, bandKey, clampInto, clockRangeText,
  clockText, coverageRequest, defaultViewport, dimSegments, dragThresholdPx, grabTolerancePx,
  grabsMarker, litSegments, panWithin, pinchFactor, pinchSpread, placeOn, regionFromDrag, sameBand,
  spanOf, spectrumExtent, stripCells, surveyCells, surveyViewport, timeExtent, timelineRequest,
  unobservedCount, valueAt, zoomWithin, type CoverageCell, type CoverageResponse, type Range,
} from "../src/navigators";
import { CMAP_GLSL, CMAP_STOPS, cmapBytes } from "../src/cmap";
import { captureWindow, currentSpan } from "../src/app/capture/timeline";
import { centreInitial, setNavigation } from "../src/app/centre/slice";
import { applyDeviceAction, retuneAction, setLiveView, setRetuneOffer } from "../src/app/centre/view";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { goLive, initialState, reviewAt, setCaptureWindow } from "../src/app/state";

const near = (a: number, b: number, eps = 1e-6) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

/** The grid a HackRF-class front end reports on `GET /api/navigation` (docs/api.md). */
const GRID: NavigationGrid = {
  frequency: {
    device_id: "hackrf:0000000000000000fake0000000000ab", driver: "hackrf-one", controllable: true,
    ranges_hz: [[1e6, 6e9]], center_step: "uniform", center_step_hz: 30e6 / 2 ** 20,
    spans_hz: { min: 2e6, max: 20e6 }, max_live_span_hz: 20e6,
    current: { center_hz: 100e6, span_hz: 2.4e6 },
  },
  time: {
    tiers: [
      { level: 0, t_cell_s: 1, f_cell_hz: 6250, max_age_s: 3600 },
      { level: 1, t_cell_s: 60, f_cell_hz: 12500, max_age_s: null },
    ],
    min_t_cell_s: 1, max_t_cell_s: 604800, latest_s: 1_789_300_920,
  },
};

const G: ax.Geometry = { centerHz: 100e6, bandwidthHz: 2.4e6, bins: 1024 };

/**
 * T-418: the window a narrowing has already reached — the device's 2 Msps floor, placed so DC sits
 * clear of a selection around 100.4 MHz. This is what "there is nothing left to narrow to" looks
 * like, and the `view` branch (radio stays put) is only reachable from a state like it.
 */
const G_NARROW: ax.Geometry = { centerHz: 99.9e6, bandwidthHz: 2e6, bins: 4096 };

/** The FFT ladder the server reports in `display_limits` (hk-pipeline's `DISPLAY_FFT_MIN`/`MAX`). */
const FFT = { fft_size_min: 64, fft_size_max: 65_536 };
/** What the display is on now, and the pixels a selection is drawn across. */
const DET = { bounds: FFT, currentBins: 1024, wantBins: 1200 };

// ---------------------------------------------------------------------------
// The frequency navigator spans the device-available spectrum
// ---------------------------------------------------------------------------

test("T-340: the frequency navigator spans the whole device-available spectrum, not the tuned window", () => {
  const ext = spectrumExtent(GRID.frequency)!;
  assert.deepEqual(ext, { lo: 1e6, hi: 6e9 });
  // Not the 2.4 MHz capture window, which is four parts in ten thousand of it.
  assert.ok(ext.hi - ext.lo > 1000 * GRID.frequency!.current!.span_hz);
  // Disjoint ranges make one axis to travel along; the gaps are the snap's business to refuse.
  assert.deepEqual(spectrumExtent({ ranges_hz: [[24e6, 1.766e9], [3e9, 6e9]], center_step_hz: 1 }), { lo: 24e6, hi: 6e9 });
  // No grid reported (a replay has no front end): nothing to draw, and no invented span.
  assert.equal(spectrumExtent(null), null);
  assert.equal(spectrumExtent({ ranges_hz: [], center_step_hz: 1 }), null);
});

// ---------------------------------------------------------------------------
// Lit segments come from the reported window list
// ---------------------------------------------------------------------------

test("T-340: lit segments are the windows the backend listed — a `current` without a list lights nothing", () => {
  const ext = spectrumExtent(GRID.frequency);

  // One reported window → one segment, placed where the backend said it is.
  const one = activeWindows({
    windows: [{
      device_id: "hackrf:abc", driver: "hackrf-one",
      center_hz: 100e6, span_hz: 2.4e6, f_lo_hz: 98.8e6, f_hi_hz: 101.2e6,
    }],
  });
  assert.equal(one.length, 1);
  const seg = litSegments(one, ext);
  assert.equal(seg.length, 1);
  assert.equal(seg[0].deviceId, "hackrf:abc");
  near(seg[0].loHz, 98.8e6);
  near(seg[0].startPct, ((98.8e6 - 1e6) / (6e9 - 1e6)) * 100, 1e-9);

  // THE CONTROL. A body that reports the tuned state but no window list lights **nothing**. If the
  // code derived a one-element list from `frequency.current`, this would be one segment — which is
  // a window count nothing measured, on a system whose front-end count is a fact about the run.
  assert.deepEqual(activeWindows({ ...GRID } as never), []);
  assert.deepEqual(litSegments(activeWindows(null), ext), []);
  assert.deepEqual(litSegments(activeWindows({ windows: [] }), ext), []);

  // N reported → N segments, in the order given, with no merging of adjacent windows: two front
  // ends on touching bands are two captures, not one.
  const two = activeWindows({
    windows: [
      { device_id: "hackrf:a", driver: "hackrf-one", center_hz: 100e6, span_hz: 20e6, f_lo_hz: 90e6, f_hi_hz: 110e6 },
      { device_id: "rtlsdr:b", driver: "rtl-sdr", center_hz: 120e6, span_hz: 20e6, f_lo_hz: 110e6, f_hi_hz: 130e6 },
    ],
  });
  const segs = litSegments(two, ext);
  assert.deepEqual(segs.map((s) => s.deviceId), ["hackrf:a", "rtlsdr:b"]);
  assert.notEqual(segs[0].startPct, segs[1].startPct);

  // A window outside the navigator's extent is omitted, never clamped to an edge — that would put
  // a capture where it is not. An unidentified front end keeps a null id rather than a placeholder.
  assert.deepEqual(litSegments(activeWindows({
    windows: [{ center_hz: 12e9, span_hz: 2e6, f_lo_hz: 11.999e9, f_hi_hz: 12.001e9 }],
  }), ext), []);
  assert.equal(activeWindows({ windows: [{ center_hz: 1e8, span_hz: 2e6, f_lo_hz: 99e6, f_hi_hz: 101e6 }] })[0].deviceId, null);

  // A window narrower than a pixel of a 6 GHz bar is still drawn: it is the thing the bar exists to
  // show. The drawn size is a floor; the measured edges are kept unchanged beside it.
  const narrow = litSegments(activeWindows({
    windows: [{ center_hz: 100e6, span_hz: 2e3, f_lo_hz: 99_999_000, f_hi_hz: 100_001_000 }],
  }), ext)[0];
  assert.ok(narrow.sizePct >= 0.5 && narrow.hiHz - narrow.loHz === 2000);
});

// ---------------------------------------------------------------------------
// T-392: region-select on the frequency navigator RETUNES — the one navigator action that
// commands the radio.
//
// The user's invariant (CLAUDE.md, the frequency navigator): *"Selecting a region on it retunes the
// front end to cover that region (snapped to an achievable config), not merely offering a retune —
// this is the one navigator action that commands the radio, because unlike time (always a view over
// already-captured data) a frequency outside the current window can only be reached by tuning
// there. An error appears only when no achievable configuration can capture the region."*
//
// The whole design is one boundary, and both sides of it are asserted below:
//
//   inside the tuned window  → a pure view zoom, **zero** device calls;
//   outside it               → **exactly one** retune, with the computed config asserted by value.
//
// The controls that stop a degenerate implementation passing: a span the ladder could cover with a
// *wider* entry must still pick the smallest one, and each of the three refusals is paired with an
// adjacent region that must retune rather than refuse — the error path is where over-refusal hides.
// ---------------------------------------------------------------------------

/** Region between two frequencies, as a drag on the bar produces it. */
const regionOf = (lo: number, hi: number) => {
  const ext = spectrumExtent(GRID.frequency)!;
  return regionFromDrag(ext, (lo - ext.lo) / (ext.hi - ext.lo), (hi - ext.lo) / (ext.hi - ext.lo))!;
};

// ---------------------------------------------------------------------------
// The time navigator's extent is the capture window, not the history horizon
// ---------------------------------------------------------------------------

test("T-340: the time navigator's extent IS the capture window (T-338 span_s), never the history horizon", () => {
  const t1 = 1_789_300_920;
  // The ring holds 90 s. The spectrum-history pyramid behind it reaches back a week — and the
  // navigation grid says so, right there in the same session.
  const body = {
    window: { enabled: true, retention_s: 90, t0_s: t1 - 90, t1_s: t1, span_s: 90, buffered: { t0_s: t1 - 40, t1_s: t1 } },
  };
  const ext = timeExtent(captureWindow(body))!;
  assert.equal(ext.hi - ext.lo, 90, "the bar spans the ring's configured retention");
  assert.equal(ext.hi, t1, "and ends at the capture clock's live edge");
  assert.equal(GRID.time!.max_t_cell_s, 604800); // the other horizon, 6720× longer, is not it
  assert.ok(ext.hi - ext.lo < GRID.time!.max_t_cell_s);

  // THE CONTROL. Reconfigure the retention and the extent follows it — a constant that happened to
  // equal one retention would pass a single-value assertion.
  const longer = timeExtent(captureWindow({ window: { ...body.window, t0_s: t1 - 7200, span_s: 7200 } }))!;
  assert.equal(longer.hi - longer.lo, 7200);

  // No window reported → no extent. The bar scrubs nothing rather than offering times with no
  // capture behind them (the failure T-338 removed: a 48 h bar over a two-minute ring).
  assert.equal(timeExtent(null), null);
  assert.equal(timeExtent(captureWindow({ window: null })), null);
  assert.equal(timeExtent({ t0S: t1, t1S: t1, spanS: 0 }), null);

  // And the module never reads the history horizon's fields at all.
  const src = readFileSync("src/navigators.ts", "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  for (const field of ["latest_s", "max_age_s", "min_t_cell_s", "max_t_cell_s"]) {
    assert.ok(!src.includes(field), `the time navigator must not size itself from ${field}`);
  }
});

// ---------------------------------------------------------------------------
// THE CONTROL: panning either navigator reaches no device route
// ---------------------------------------------------------------------------

/** A context whose client records every call, so "reached the device" is observable (T-343). */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: {
      ...s.device, loaded: true, live, deviceId: "hackrf:0000000000000000fake0000000000ab",
      // T-341's centre axis, so a navigator retune snaps to the front end's synthesiser grid.
      centerGrid: { ranges_hz: GRID.frequency!.ranges_hz, center_step_hz: GRID.frequency!.center_step_hz },
    },
    live: { ...s.live, centerHz: 100e6, bandwidthHz: 2.4e6, bins: 1024, view: { loHz: 99.5e6, hiHz: 100.5e6 } },
  }));
  store.set(setNavigation(GRID, activeWindows({
    windows: [{ device_id: "hackrf:abc", driver: "hackrf-one", center_hz: 100e6, span_hz: 2.4e6, f_lo_hz: 98.8e6, f_hi_hz: 101.2e6 }],
  })));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

test("T-340 control: panning and zooming the time navigator reaches no device route", () => {
  const { ctx, calls } = deviceSpyCtx();
  const t1 = 1_789_300_920;
  const ext = timeExtent({ t0S: t1 - 600, t1S: t1, spanS: 600 })!;

  // Scrub the whole bar, then zoom the time axis in and out over its full range.
  for (const f of [0, 0.25, 0.5, 0.75, 1]) ctx.store.set(reviewAt(valueAt(ext, f), 30));
  for (const factor of [0.01, 0.5, 2, 100]) {
    const z = zoomWithin(ext, { lo: t1 - 30, hi: t1 }, 0.5, factor, 1e-3);
    ctx.store.set(reviewAt(z.hi, z.hi - z.lo));
  }
  assert.deepEqual(calls, [], "no time gesture may reach the control API");
  assert.equal(ctx.store.get().time.live, false, "the time cursor moved");
});

// ---------------------------------------------------------------------------
// Placement and range arithmetic
// ---------------------------------------------------------------------------

test("T-340: placement clips to the extent, clamps pans, and keeps zooms inside the bar", () => {
  const ext = { lo: 0, hi: 100 };
  near(placeOn(ext, 25, 75)!.startPct, 25);
  near(placeOn(ext, 25, 75)!.sizePct, 50);
  // Partly outside: clipped, not scaled away.
  near(placeOn(ext, -50, 10)!.startPct, 0);
  near(placeOn(ext, -50, 10)!.sizePct, 10);
  assert.equal(placeOn(ext, 200, 300), null);
  assert.equal(placeOn(null, 1, 2), null);
  // A drawn floor keeps a zero-width mark visible without claiming a width.
  near(placeOn(ext, 50, 50, 2)!.sizePct, 2);

  // Pan: keeps the width, clamps to bounds, and reports the overshoot rather than acting on it.
  const p = panWithin(ext, { lo: 10, hi: 20 }, 200);
  assert.deepEqual(p.range, { lo: 90, hi: 100 });
  near(p.overflow, 120);
  assert.deepEqual(panWithin(ext, { lo: 10, hi: 20 }, 5), { range: { lo: 15, hi: 25 }, overflow: 0 });

  // Zoom: about the pointer, never narrower than the floor nor wider than the bar.
  const z = zoomWithin(ext, { lo: 0, hi: 100 }, 0.5, 2, 1);
  assert.deepEqual(z, { lo: 25, hi: 75 });
  assert.deepEqual(zoomWithin(ext, { lo: 0, hi: 100 }, 0.5, 0.01, 1), { lo: 0, hi: 100 });
  assert.deepEqual(zoomWithin(ext, { lo: 40, hi: 60 }, 0, 1e9, 4), { lo: 40, hi: 44 });
  assert.deepEqual(clampInto(ext, { lo: 95, hi: 115 }), { lo: 80, hi: 100 });
  near(valueAt(ext, 0.25), 25);
  near(valueAt(ext, 5), 100, 0); // clamped, never extrapolated off the bar
});

// ---------------------------------------------------------------------------
// T-367: the two bars control DIFFERENT axes
// ---------------------------------------------------------------------------
//
// The user's correction to T-340: *"the left vertical bar is the TIME navigator: it selects the
// time range and shows a compressed history waterfall **of the currently-selected frequency range
// only** (not the whole spectrum) — it never changes frequency. The bottom horizontal bar is the
// FREQUENCY navigator … it never scrubs time."*
//
// The property is a pair, and both halves are asserted below:
//
//   a. the time navigator's overview **changes** when the selected frequency range changes;
//   b. the frequency navigator's content does **not** change when the time selection changes.
//
// The control for each gesture is stronger than "looks right": the *other* axis's slice comes out
// of the store **bit-identical** — the same object, by `Object.is`, not merely deep-equal.

/** The state the time navigator derives its band from, as the mount reads it. */
const bandOf = (s: ReturnType<typeof initialState>) => currentSpan({ live: s.live.view, device: s.device });

test("T-367 property (a): the time navigator's overview is scoped to the SELECTED frequency range, and follows it", () => {
  const { ctx } = deviceSpyCtx();
  const cols = 160, rows = 6;

  // The main view is zoomed inside the tuned 2.4 MHz window: 99.5–100.5 MHz.
  const first = bandOf(ctx.store.get())!;
  assert.deepEqual(first, { loHz: 99.5e6, hiHz: 100.5e6 });
  const reqA = timelineRequest(first, cols, rows);
  assert.ok(reqA.includes(`f_lo=${99.5e6}`) && reqA.includes(`f_hi=${100.5e6}`), reqA);

  // THE CONTROL that rules out "pass the tuned band and call it scoped". The device is tuned to
  // 100 MHz at 2.4 Msps, so the tuned band is 98.8–101.2 MHz — and that is NOT what is asked for.
  assert.ok(!reqA.includes(`f_lo=${98.8e6}`), `the request must carry the selected range, not the tuned band: ${reqA}`);

  // Move the frequency selection: the request follows it. The two are different requests, which is
  // the whole property — an overview that ignored the range would be byte-identical here.
  ctx.store.set(setLiveView({ loHz: 100.1e6, hiHz: 100.3e6 }));
  const second = bandOf(ctx.store.get())!;
  const reqB = timelineRequest(second, cols, rows);
  assert.notEqual(reqA, reqB);
  assert.ok(reqB.includes(`f_lo=${100.1e6}`) && reqB.includes(`f_hi=${100.3e6}`), reqB);
  assert.equal(sameBand(first, second), false);
  assert.notEqual(bandKey(first), bandKey(second));

  // THE OTHER CONTROL: with no range selected the bar asks for no picture — it does NOT widen to
  // the whole spectrum. `/api/timeline` answers a null grid with no region, and drawing nothing is
  // the honest answer; the device-available spectrum's edges appear nowhere in the request.
  const none = timelineRequest(null, cols, rows);
  assert.ok(!none.includes("f_lo") && !none.includes("f_hi"), none);
  const ext = spectrumExtent(GRID.frequency)!;
  for (const edge of [ext.lo, ext.hi]) assert.ok(!none.includes(String(edge)), `${none} names the spectrum extent`);
  // Both still ask for the same cells: the shape of the picture is the bar's, its span is not.
  for (const r of [reqA, reqB, none]) assert.ok(r.includes(`columns=${cols}`) && r.includes(`rows=${rows}`), r);

  // A degenerate range is no range rather than a zero-width request.
  assert.equal(bandKey({ loHz: 100e6, hiHz: 100e6 }), "");
  assert.equal(timelineRequest({ loHz: 100e6, hiHz: 100e6 }, cols, rows), none);
  assert.equal(sameBand(null, null), true);

  // And with nothing zoomed the band falls back to what the device is tuned to — still a frequency
  // range, still not the spectrum.
  ctx.store.set((s) => ({
    live: { ...s.live, view: null },
    device: { ...s.device, centerHz: 100e6, sampleRateHz: 2.4e6 },
  }));
  assert.deepEqual(bandOf(ctx.store.get()), { loHz: 98.8e6, hiHz: 101.2e6 });
  // With neither, there is no range — and that is *no picture*, never the whole spectrum.
  ctx.store.set((s) => ({ device: { ...s.device, centerHz: null, sampleRateHz: null } }));
  assert.equal(bandOf(ctx.store.get()), null);
  assert.equal(timelineRequest(bandOf(ctx.store.get()), cols, rows), none);
});

// ---------------------------------------------------------------------------
// T-368: the frequency navigator's survey strip, and grey meaning genuinely unobserved
// ---------------------------------------------------------------------------
//
// The user's invariant: *"the waterfall shows the data that exists for the selected (time,
// frequency); grey means genuinely unobserved … the frequency navigator's survey view is built from
// that same coverage."*
//
// The property here is the client half: **the three states stay three on the way to the pixel.**
// Observed-and-quiet and never-observed must not arrive as the same value, and "not asked yet" must
// not arrive as "nothing observed". Each has the control that fails if the distinction collapses.

test("T-368: an unobserved cell and an observed-quiet cell are different values, not the same null", () => {
  // Exactly what `GET /api/coverage` serves (docs/api.md): the unobserved cell carries NO
  // measurement keys, and the quiet cell carries a real one at the bottom of the scale.
  const body = {
    grid: { cells: 4, f_lo_hz: 1e6, f_cell_hz: 1e6 },
    any: { cells: [
      { state: "observed", shade: 0 },        // 2: looked, and it was quiet — a finding
      { state: "observed", shade: 0.93 },     // 1: looked, and there was energy
      { state: "unobserved" },                // 3: nothing ever looked — grey
      { state: "observed", shade: null },     // sampled, level not retained — neither of the above
    ] },
  } as const;
  const cells = surveyCells(body as unknown as CoverageResponse);
  assert.equal(cells.length, 4);

  // The pair that gets collapsed, kept apart: a measured nothing is not an absence of measurement.
  assert.notDeepEqual(cells[0], cells[2]);
  assert.equal(cells[0].state, "observed");
  assert.equal(cells[0].shade, 0);
  assert.equal(cells[2].state, "unobserved");
  assert.equal("shade" in cells[2], false, "an unobserved cell carries no measurement key to misread");

  // Only the never-observed cell is grey. The quiet one is not, and neither is the one whose level
  // was not retained — three cells, three treatments.
  assert.equal(cells.filter((c) => c.state !== "observed").length, 1);
  assert.equal(unobservedCount(cells), 1);

  // The control: collapse the states the way a naive client would — read `shade ?? 0` and call the
  // low ones quiet — and the never-observed cell becomes indistinguishable from the quiet one.
  const collapsed = cells.map((c) => c.shade ?? 0);
  assert.equal(collapsed[0], collapsed[2], "the collapse the assertions above forbid");
});

test("T-368: an unasked strip reads as unknown, never as nothing observed", () => {
  // "Not asked yet" is not a finding. `null`, not 0 — the same rule as `coverageText`'s.
  assert.equal(unobservedCount([]), null);
  assert.deepEqual(surveyCells(null), []);
  assert.deepEqual(surveyCells(undefined), []);
  assert.deepEqual(surveyCells({ grid: null, any: null } as CoverageResponse), []);
  assert.deepEqual(surveyCells({ any: { cells: null } } as unknown as CoverageResponse), []);
  // The control: a strip that WAS served, and every cell of which was never observed, reports its
  // count — so the `null` above is genuinely "unknown" and not "none".
  const all = [{ state: "unobserved" }, { state: "unobserved" }] as CoverageCell[];
  assert.equal(unobservedCount(all), 2);
});

test("T-368: the survey strip asks about the spectrum extent the bar spans, and nothing else", () => {
  const ext = spectrumExtent(GRID.frequency)!;
  const path = coverageRequest(ext, 512)!;
  // The range asked for is the bar's own extent — the device-available spectrum from the grid —
  // so every drawn cell is of a frequency the bar actually covers.
  assert.match(path, /^\/api\/coverage\?/);
  assert.equal(new URLSearchParams(path.split("?")[1]).get("f_lo"), String(ext.lo));
  assert.equal(new URLSearchParams(path.split("?")[1]).get("f_hi"), String(ext.hi));
  assert.equal(new URLSearchParams(path.split("?")[1]).get("cells"), "512");
  // No extent, no request: the bar draws no strip rather than one over an assumed range.
  assert.equal(coverageRequest(null, 512), null);
  assert.equal(coverageRequest({ lo: 10, hi: 10 }, 512), null);
  assert.equal(coverageRequest(ext, 0), null);
  // With no window it sends none, and the route then uses the server's live capture window.
  assert.ok(!path.includes("t0="), "no window asked for, the route's own default stands");
});

test("T-379: the survey is of the WINDOW on screen — it carries the time range without scrubbing it", () => {
  // The bar is the frequency axis and still never scrubs time (T-367): it reads the window, it does
  // not set it. But it is a view over the one (time × frequency) window, so it has to *ask about*
  // that window. Omitting `t0`/`t1` pinned it to the server's live edge, so scrubbed an hour back
  // the bottom bar reported coverage for a time nobody was looking at — and greyed bands that had
  // in fact been observed at the reviewed instant.
  const ext = spectrumExtent(GRID.frequency)!;
  const w = { t0: 1_789_297_727, t1: 1_789_297_847 };
  const q = new URLSearchParams(coverageRequest(ext, 512, w)!.split("?")[1]);
  assert.equal(q.get("t0"), String(w.t0), "the window's start, on the capture clock");
  assert.equal(q.get("t1"), String(w.t1));
  assert.equal(q.get("f_lo"), String(ext.lo), "and still the bar's own frequency extent");

  // Two different windows produce two different requests — the property that makes a scrub visible
  // at all. Without it, every window would be answered with the live edge's cells.
  const other = { t0: w.t0 - 3600, t1: w.t1 - 3600 };
  assert.notEqual(coverageRequest(ext, 512, w), coverageRequest(ext, 512, other));

  // A malformed window is not sent at all, rather than being sent as a range that selects nothing.
  for (const bad of [{ t0: 5, t1: 5 }, { t0: 9, t1: 1 }, { t0: NaN, t1: 2 }]) {
    assert.ok(!coverageRequest(ext, 512, bad)!.includes("t0="), `${JSON.stringify(bad)} is not a window`);
  }
});

// ---------------------------------------------------------------------------
// T-376: the frequency bar has a viewport of its own, centred on the tune
// ---------------------------------------------------------------------------
//
// The user: *"the bottom FREQUENCY bar surveys the whole device-available range but its VIEW is
// CENTRED ON THE CURRENT TUNE CENTRE by default … and you wheel-zoom out to see more of the range
// or in to narrow it."*
//
// Before this the bar's extent was the whole reported spectrum, fixed — so on a 1 MHz–6 GHz front
// end the 2.4 MHz capture window was four ten-thousandths of the bar: invisible, not off-centre.

test("T-376: the frequency bar opens centred on the tune, not on the middle of the spectrum", () => {
  const b = spectrumExtent(GRID.frequency)!;
  const cur = GRID.frequency!.current!;
  const v = defaultViewport(b, cur.center_hz, cur.span_hz)!;

  // Centred on the tune, to within the clamp.
  near((v.lo + v.hi) / 2, cur.center_hz, 1);
  // …which is emphatically NOT the middle of the reported range, and that is the whole point.
  assert.ok(Math.abs((v.lo + v.hi) / 2 - (b.lo + b.hi) / 2) > spanOf(b) / 4);
  // The capture window is now a readable fraction of the bar rather than a vanishing one.
  const before = cur.span_hz / spanOf(b);         // the whole-spectrum bar: invisible
  const after = cur.span_hz / spanOf(v);          // the viewport: visible
  assert.ok(before < 1e-3, `${before} — the pre-T-376 bar`);
  assert.ok(after > 0.01, `${after} — the window must be visible on the bar`);
  // It stays inside the device's own reported bounds; nothing here invents a range.
  assert.ok(v.lo >= b.lo && v.hi <= b.hi);

  // No tune reported: the honest frame is the whole reported range, never a guessed middle.
  assert.deepEqual(defaultViewport(b, null, null), { ...b });
  assert.deepEqual(defaultViewport(b, cur.center_hz, 0), { ...b });
  assert.equal(defaultViewport(null, cur.center_hz, cur.span_hz), null);
});

test("T-376: an untouched viewport follows a retune; a viewport the user framed does not", () => {
  const b = spectrumExtent(GRID.frequency)!;
  const cur = GRID.frequency!.current!;
  const first = surveyViewport(b, null, false, cur.center_hz, cur.span_hz)!;
  near((first.lo + first.hi) / 2, cur.center_hz, 1);

  // The radio moves. An untouched frame answers "where am I", so it moves with it.
  const moved = cur.center_hz * 4;
  const followed = surveyViewport(b, first, false, moved, cur.span_hz)!;
  near((followed.lo + followed.hi) / 2, moved, 1);
  assert.notDeepEqual(followed, first);

  // The user frames a region by wheel-zooming. Now the radio moves again — and the frame they made
  // stays where they put it, because re-centring under them would undo the gesture.
  const framed = zoomWithin(b, first, 0.5, 8, cur.span_hz);
  const held = surveyViewport(b, framed, true, moved * 2, cur.span_hz)!;
  assert.deepEqual(held, framed);
  // Still clamped into the device's bounds, whatever the user did.
  const wild = surveyViewport(b, { lo: b.lo - spanOf(b), hi: b.lo - 1 }, true, moved, cur.span_hz)!;
  assert.ok(wild.lo >= b.lo && wild.hi <= b.hi);
});

test("T-376: wheel-zooming the frequency bar changes only the bar's frame, and reaches no device", async () => {
  const { ctx, calls } = deviceSpyCtx();
  const b = spectrumExtent(GRID.frequency)!;
  const cur = GRID.frequency!.current!;
  const before = ctx.store.get();
  let v = surveyViewport(b, null, false, cur.center_hz, cur.span_hz)!;

  // What the mount's wheel handler does, at every zoom the user can reach: out to the whole range
  // and back in to one capture window.
  for (const factor of [0.01, 0.5, 2, 100, 1e6]) {
    v = zoomWithin(b, v, 0.5, factor, cur.span_hz);
    assert.ok(v.lo >= b.lo && v.hi <= b.hi, "never outside the reported bounds");
    assert.ok(spanOf(v) >= cur.span_hz - 1e-6, "never narrower than one capture window");
  }
  // Zoomed all the way out (a factor below 1 widens), the bar shows the whole reported range —
  // which is what has to be filled honestly, and why the coverage map is the other half of this.
  assert.ok(spanOf(zoomWithin(b, v, 0.5, 1e-9, cur.span_hz)) >= spanOf(b) - 1e-6);
  // …and all the way in, it stops at one capture window rather than claiming finer survey detail.
  assert.ok(Math.abs(spanOf(zoomWithin(b, v, 0.5, 1e9, cur.span_hz)) - cur.span_hz) < 1e-6);

  // The two properties T-340 and T-343 put on this bar, restated for the new gesture.
  assert.deepEqual(calls, [], "a bar zoom must never reach the control API");
  assert.ok(Object.is(ctx.store.get().live, before.live), "a bar zoom must not move the main view");
  assert.ok(Object.is(ctx.store.get().time, before.time), "a bar zoom must not scrub time");
});

/** The presence socket stub, as T-388's own suite uses one: the point is what does and does not get
 * constructed, so the class only has to record that. */
class LiveSocket {
  static open: LiveSocket[] = [];
  binaryType = "";
  onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null = null;
  onclose: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) { LiveSocket.open.push(this); }
  close() { this.closed = true; this.onclose?.(); }
}

/** Every `.ts` under `dir`, recursively (the same walk `app-centre.test.ts` uses for its own
 * source-level rule). */
function walkSrc(dir: string): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = `${dir}/${e.name}`;
    if (e.isDirectory()) out.push(...walkSrc(p));
    else if (e.name.endsWith(".ts")) out.push(p);
  }
  return out;
}

test("T-347: no file under src/ names the retired pause routes", () => {
  const retired = ["/api/control/pause", "/api/control/resume"];
  const callers = walkSrc("src").filter((f) => {
    const src = readFileSync(f, "utf8")
      // Comments explaining why the route is gone are the point, not a caller.
      .replace(/\/\*[\s\S]*?\*\//g, "")
      .replace(/^\s*\/\/.*$/gm, "")
      .replace(/^\s*\*.*$/gm, "");
    return retired.some((r) => src.includes(r));
  });
  assert.deepEqual(callers, [], "a run-wide pause is one viewer freezing all the others");
});

// ---------------------------------------------------------------------------
// T-397 / T-405 / T-411: how the two bars RENDER
// ---------------------------------------------------------------------------
//
// The user, live-testing: *"the left overview is badly pixelated"*, *"the bottom strip is one row
// stretched"*, *"there are yellow/red peaks not showing because averaging washes them out"*, and
// *"the strips are cyan/teal, not the waterfall's colours"*. Four complaints, three of them about
// this file's callers and one about the pipeline (`hk-pipeline`'s `history_welch`, whose own test
// measures the max-hold end to end).
//
// The three rendering properties, each with the control that stops a degenerate fix passing:
//
//  1. **The buffer is the bar's own size, not a constant.** `stripCells` returns one cell per pixel
//     — control: it must *change* with the pixel count, so a fix that upsized the canvas while
//     still asking for six cells fails.
//  2. **One colour ramp in `ui/src`.** The stops live in `cmap.ts`, the shader is generated from
//     them, and no other module under `src/` may contain a ramp — control: the distinctive stop
//     literals appear in exactly one file.
//  3. **The ramp actually reaches yellow, red and white.** The old strip ramp was cyan at every
//     input, which is why a peak could not look like one — control: `cmapBytes(0.75)` is yellow and
//     `cmapBytes(1)` is white, so a cyan-only ramp cannot pass.

test("T-411/T-397: a strip's render buffer is one cell per pixel of the bar, never a constant", () => {
  // The bug: 6 frequency cells stretched across an 80 px bar (the time navigator) and 1 row
  // stretched down a 64 px bar (the frequency navigator).
  assert.equal(stripCells(80, 512), 80);
  assert.equal(stripCells(64, 4096), 64);
  assert.equal(stripCells(400, 4096), 400);
  // THE CONTROL: it must move with the bar. A function that returned a constant — the defect —
  // gives the same answer for both of these.
  assert.notEqual(stripCells(80, 512), stripCells(12, 512));
  // Capped at what the route will answer, and never zero.
  assert.equal(stripCells(9000, 512), 512);
  assert.equal(stripCells(0.4, 512), 1);
  // No layout yet (before first paint) asks for the cap, not one cell: asking for one cell would
  // bake the blockiness back in for the first poll.
  assert.equal(stripCells(0, 512), 512);
  assert.equal(stripCells(NaN, 256), 256);
});

// ---------------------------------------------------------------------------
// T-412: on the time axis the wheel PANS; span-zoom is the secondary gesture
// T-407: the bars on touch — one arithmetic, bigger targets, no page scroll
// ---------------------------------------------------------------------------
//
// The user's correction: *"the TIME axis wheel should PAN / SCROLL THROUGH TIME — wheel up/down
// moves the viewed window earlier/later, keeping the duration/span FIXED — NOT zoom the time
// window. The user scrolls through time constantly and rarely resizes, so time-span zoom moves to a
// deliberate secondary gesture (Ctrl/Cmd + wheel, or drag-select-to-zoom). Direction matches the
// waterfall time axis: scroll up = back toward older. Frequency-axis wheel stays zoom."*
//
// And beside it: *"the left + bottom scrubber bars are hard to use on touch (everything else is
// fine on mobile) — larger hit targets, proper touch drag/pinch, no accidental page scroll."*
//
// They are one suite because they are one requirement: **a pinch is the touch equivalent of the
// wheel and a touch drag is the existing pan**, so whatever pan/zoom split T-412 lands, every input
// device must resolve to the same arithmetic rather than growing a second copy of it. The controls
// below are therefore not "the gestures work" — they are:
//
//  - the span **survives** a pan into either end (the mirror of the bug being replaced);
//  - a pinch and a wheel produce **the same factor** and land in the same function;
//  - and changing what a gesture *means* changed nothing about what it can **reach** (T-340).

/** The capture window these use: ten minutes of retained IQ ending at `T1`. */
const T1 = 1_789_300_920;
const timeExt = (): Range => timeExtent({ t0S: T1 - 600, t1S: T1, spanS: 600 })!;
