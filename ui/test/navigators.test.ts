// T-340: the two edge navigators, one parallel to each waterfall axis.
//
// The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"): *time runs down the
// waterfall and frequency across it, so the time navigator is a vertical bar on the side (an
// overview of the retained capture window) and the frequency navigator is a horizontal bar along
// the bottom (spanning the whole surveyed / device-available spectrum, setting the centre). Each
// navigator pans and zooms its own axis; a dragged region on either zooms the main view to it. The
// frequency navigator shows every currently-active capture window as a lit segment.*
//
// Three properties, each with the control that stops a degenerate implementation passing it:
//
//  1. a dragged region on either navigator zooms the main view to it, **snapped** to the grid the
//     backend reported (T-341) — control: the same drag outside the tuned band produces an *offer*,
//     not a view, so "it always zooms" fails;
//  2. the time navigator's extent is the **capture window** (T-338's `window.span_s`), not the
//     spectrum-history horizon — control: a body whose history horizon is a thousand times longer
//     still yields the ring's span;
//  3. a pan on either navigator **reaches no device route**, at any distance — T-343's control,
//     restated for the surface the user asked to be able to set the centre from;
//  4. **(T-367)** the two bars control *different* axes — the time navigator's overview is scoped
//     to the selected frequency range and follows it, the frequency navigator's content is
//     untouched by the time selection — control: after a gesture on either bar, the *other* axis's
//     slice is the same object, by `Object.is`, and the source of each mount names none of the
//     other's writers.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as ax from "../src/axis";
import { snapState, type NavigationGrid } from "../src/navigation";
import {
  activeWindows, bandKey, clampInto, clockRangeText, clockText, coverageRequest, defaultViewport,
  dimSegments, litSegments, panWithin, placeOn, regionFromDrag, sameBand, spanOf, spectrumExtent,
  stripCells, surveyCells, surveyViewport, timeExtent, timelineRequest, unobservedCount, valueAt,
  zoomWithin, type CoverageCell, type CoverageResponse, type Range,
} from "../src/navigators";
import { CMAP_GLSL, CMAP_STOPS, cmapBytes } from "../src/cmap";
import { captureWindow, currentSpan } from "../src/app/capture/timeline";
import {
  applyFreqZoom, applyTimeTarget, freqHoverText, freqPan, freqSelectText, freqZoomTarget,
  goLiveFromNav, timeDetailText, timeDragText, timeHoverText, timePanTarget, timeWheelTarget,
  timeZoomTarget,
} from "../src/app/centre/navigators";
import { mountPresenceStream } from "../src/app/explore/presence-stream";
import { mounts } from "../src/app/centre";
import { historyWindow, sameCursor } from "../src/app/centre/review-render";
import { centreInitial, setNavigation } from "../src/app/centre/slice";
import { applyDeviceAction, retuneAction, setLiveView, setRetuneOffer } from "../src/app/centre/view";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { goLive, initialState, reviewAt } from "../src/app/state";

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
// A dragged region zooms the main view — frequency
// ---------------------------------------------------------------------------

test("T-340: a region dragged on the frequency navigator zooms the main view to it, snapped", () => {
  const ext = spectrumExtent(GRID.frequency)!;
  // Two pointer fractions of the bar → the frequency region between them.
  const fracOf = (hz: number) => (hz - ext.lo) / (ext.hi - ext.lo);
  const region = regionFromDrag(ext, fracOf(99.6e6), fracOf(100.4e6))!;
  near(region.lo, 99.6e6, 1);
  near(region.hi, 100.4e6, 1);

  // Inside the tuned band: a display zoom over live IQ. No device is involved and nothing snaps —
  // a zoom inside the window is not a capture state (T-341).
  const z = freqZoomTarget(GRID, G, region, true);
  assert.equal(z.kind, "view");
  if (z.kind !== "view") return;
  near(z.view.loHz, 99.6e6, 1);
  near(z.view.hiHz, 100.4e6, 1);
  assert.equal(z.source, "live-iq");

  // A zero-width drag (a click) selects nothing.
  assert.equal(regionFromDrag(ext, 0.5, 0.5), null);
  assert.equal(freqZoomTarget(GRID, G, null, true).kind, "none");

  // A drag wider than one capture window cannot be captured at all (T-392): no achievable config
  // covers 120 MHz on a 20 MHz front end, so it is one of the three refusals.
  const wide = regionFromDrag(ext, fracOf(80e6), fracOf(200e6))!;
  const w = freqZoomTarget(GRID, G, wide, true);
  assert.equal(w.kind, "error");
  assert.equal(w.kind === "error" ? w.reason : null, "span_too_wide");
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

test("T-392: a region OUTSIDE the tuned window retunes to the config that covers it — it does not offer", () => {
  // A region a long way from the tuned 100 MHz: the waterfall does not hold this IQ, and no view
  // state can make it. Before T-392 this left an offer and told the user to retune themselves.
  const region = regionOf(432.0e6, 432.4e6);
  const z = freqZoomTarget(GRID, G, region, true);
  assert.equal(z.kind, "retune", "a region outside the window must command the radio, not propose it");
  if (z.kind !== "retune") return;

  // The centre is on the front end's own synthesiser grid (T-341), not a rounded hertz.
  const step = GRID.frequency!.center_step_hz!;
  near(z.centerHz / step - Math.round(z.centerHz / step), 0, 1e-6);
  near(z.centerHz, snapState(GRID, 432.2e6, 400e3).centerHz!, 1e-3);
  // The span is the smallest achievable one covering the selection. This front end's rates are
  // continuous from 2 MHz, so a 400 kHz selection opens the narrowest window that holds it.
  assert.equal(z.spanHz, 2e6);
  assert.equal(z.source, "live-iq");
  assert.deepEqual(z.view, { loHz: region.lo, hiHz: region.hi });

  // With an unknown tuning step nothing snaps: the region's own centre is carried, and nothing
  // claims the device sits exactly there (`snapState` returns null).
  const unknown: NavigationGrid = {
    ...GRID, frequency: { ...GRID.frequency!, center_step: "unknown", center_step_hz: null },
  };
  const u = freqZoomTarget(unknown, G, region, true);
  near(u.kind === "retune" ? u.centerHz : NaN, 432.2e6, 1);
  assert.equal(snapState(unknown, 432.2e6, 400e3).centerHz, null);
});

test("T-392: the span chosen is the SMALLEST achievable one that covers the region, not merely one that does", () => {
  // A discrete rate ladder with a wider entry that would also cover the selection: picking by
  // "nearest" or by "the first that fits, scanning down" both give the wrong answer here.
  const ladder: NavigationGrid = {
    ...GRID,
    frequency: {
      ...GRID.frequency!,
      spans_hz: { values: [20e6, 2e6, 10e6, 8e6] }, // deliberately unsorted: order must not decide
      max_live_span_hz: 20e6,
      // A round tuning step, so the centres below land exactly on it and the rung under test is the
      // region's width rather than the width plus a fraction of a synthesiser step.
      center_step: "uniform", center_step_hz: 1e3,
    },
  };
  // 3 MHz wide at 432 MHz: 8, 10 and 20 MHz all cover it. Only 8 MHz is the smallest.
  const three = freqZoomTarget(ladder, G, { lo: 430.5e6, hi: 433.5e6 }, true);
  assert.equal(three.kind === "retune" ? three.spanHz : null, 8e6);
  // Narrower than every entry: the ladder's own floor, not the region's width.
  const tiny = freqZoomTarget(ladder, G, { lo: 432.0e6, hi: 432.1e6 }, true);
  assert.equal(tiny.kind === "retune" ? tiny.spanHz : null, 2e6);
  // Exactly one entry wide: the boundary covers, so it takes that entry rather than the next up.
  const exact = freqZoomTarget(ladder, G, { lo: 428e6, hi: 436e6 }, true);
  assert.equal(exact.kind === "retune" ? exact.spanHz : null, 8e6);
  // A hair wider than it: the next rung, and only the next rung.
  const over = freqZoomTarget(ladder, G, { lo: 428e6, hi: 436.001e6 }, true);
  assert.equal(over.kind === "retune" ? over.spanHz : null, 10e6);
  // Wider than every entry: no configuration covers it, which is refusal (2).
  const past = freqZoomTarget(ladder, G, { lo: 422e6, hi: 443e6 }, true);
  assert.equal(past.kind === "error" ? past.reason : null, "span_too_wide");

  // The smallest covering span is measured from the centre the radio will SIT ON, not the one
  // asked for. With a coarse synthesiser step the snap moves the centre, and a span chosen before
  // it would leave an edge of the selection outside the window.
  const coarse: NavigationGrid = {
    ...GRID,
    frequency: {
      ...GRID.frequency!, center_step: "uniform", center_step_hz: 1e6,
      spans_hz: { min: 1, max: 20e6 },
    },
  };
  const r = regionOf(432.4e6, 432.8e6); // centre 432.6 MHz snaps down to 432 MHz: 0.6 MHz away
  const c = freqZoomTarget(coarse, G, r, true);
  assert.equal(c.kind, "retune");
  if (c.kind !== "retune") return;
  assert.equal(c.centerHz, 433e6);
  // 433 ± span/2 must contain 432.4–432.8, so span ≥ 2 × (433 − 432.4) = 1.2 MHz — and exactly that.
  assert.equal(c.spanHz, 1.2e6);
  assert.ok(c.centerHz - c.spanHz / 2 <= r.lo + 1e-6 && c.centerHz + c.spanHz / 2 >= r.hi - 1e-6,
    "the chosen window must contain the selection");
});

test("T-392: the three refusals — and next to each, a region that must RETUNE rather than refuse", () => {
  const gf = GRID.frequency!;

  // (1) A centre outside the device's tunable range. The bar's own extent is 1 MHz–6 GHz, so the
  // out-of-range case is built from the grid rather than from the drag: a region above 6 GHz.
  const above = { lo: 6.1e9, hi: 6.2e9 };
  const e1 = freqZoomTarget(GRID, G, above, true);
  assert.equal(e1.kind === "error" ? e1.reason : null, "center_out_of_range");
  // ADJACENT: a centre just inside the top of the range retunes. The window it opens runs past
  // 6 GHz, and that is fine — the invariant bounds the *centre*, not the window's edges.
  const inside = { lo: gf.ranges_hz[0][1] - 0.2e6, hi: gf.ranges_hz[0][1] };
  const a1 = freqZoomTarget(GRID, G, inside, true);
  assert.equal(a1.kind, "retune", "a centre inside the range must retune, however close to the edge");
  // …and the same at the bottom of the range.
  const low = { lo: gf.ranges_hz[0][0], hi: gf.ranges_hz[0][0] + 0.2e6 };
  assert.equal(freqZoomTarget(GRID, G, low, true).kind, "retune");

  // (2) A span wider than one live window. 20 MHz is this front end's instantaneous bandwidth.
  const max = gf.max_live_span_hz!;
  const e2 = freqZoomTarget(GRID, G, { lo: 432e6 - max, hi: 432e6 + max }, true);
  assert.equal(e2.kind === "error" ? e2.reason : null, "span_too_wide");
  // ADJACENT: exactly `max_live_span_hz` wide is one window's worth and must retune — the boundary
  // counts as inside, the same direction `detailOf` errs in.
  const a2 = freqZoomTarget(GRID, G, { lo: 432e6 - max / 2, hi: 432e6 + max / 2 }, true);
  assert.equal(a2.kind, "retune", "a selection exactly one window wide is capturable");
  assert.equal(a2.kind === "retune" ? a2.spanHz : null, max);

  // (3) On a replay, outside the recording's extent. A replay reports the recording as its band
  // (`replay_capabilities`), so this is the same `ranges_hz` check against a much smaller range.
  const recording: NavigationGrid = {
    ...GRID,
    frequency: {
      ...gf, controllable: false, device_id: null, driver: "sigmf-replay",
      ranges_hz: [[99e6, 101e6]], center_step: "unknown", center_step_hz: null,
      spans_hz: { values: [2e6] }, max_live_span_hz: 2e6,
      current: { center_hz: 100e6, span_hz: 2e6 },
    },
  };
  // The replay's own window is narrower than the recording's extent, so "outside the played window"
  // and "outside the recording" are different places and the two answers can be told apart.
  const Gr: ax.Geometry = { centerHz: 100e6, bandwidthHz: 0.4e6, bins: 1024 };
  const e3 = freqZoomTarget(recording, Gr, { lo: 432e6, hi: 432.4e6 }, false);
  assert.equal(e3.kind === "error" ? e3.reason : null, "center_out_of_range");
  assert.match(e3.kind === "error" ? e3.text : "", /this recording covers/);
  // ADJACENT (inside the recording): behaves. A region inside the played window is a view zoom,
  // with no device involved on either side of the boundary.
  const a3 = freqZoomTarget(recording, Gr, { lo: 99.9e6, hi: 100.1e6 }, false);
  assert.equal(a3.kind, "view");
  // Inside the recording's extent but outside the played window: not a refusal about the region —
  // there is simply no radio on a replay, and it says so rather than posting a doomed retune.
  const a3b = freqZoomTarget(recording, Gr, { lo: 100.6e6, hi: 100.9e6 }, false);
  assert.equal(a3b.kind === "error" ? a3b.reason : null, "not_live");

  // A region with no width is still nothing at all, not an error.
  assert.equal(freqZoomTarget(GRID, G, null, true).kind, "none");
  // And the live-device error text names what the front end can tune, from the wire.
  assert.match(e1.kind === "error" ? e1.text : "", /the front end can tune/);
});

test("T-392: THE BOUNDARY — inside the window is zero device calls, outside it is one retune with the computed config", async () => {
  const { ctx, calls } = deviceSpyCtx();
  const v0 = ctx.store.get().live.view!;

  // INSIDE the tuned 98.8–101.2 MHz window: "look closer". A pure view zoom, and the radio is not
  // touched on any path — this is the half of the gesture that must never become a device action.
  const inside = regionOf(99.6e6, 100.4e6);
  await applyFreqZoom(ctx, freqZoomTarget(GRID, G, inside, true));
  assert.deepEqual(calls, [], "a region inside the tuned window must reach no device route");
  assert.notDeepEqual(ctx.store.get().live.view, v0, "…and it is not that nothing happened: the view zoomed");
  near(ctx.store.get().live.view!.loHz, 99.6e6, 1);
  near(ctx.store.get().live.view!.hiHz, 100.4e6, 1);
  assert.equal(ctx.store.get().live.retuneOffer, null, "no offer either — there is nothing to offer");

  // OUTSIDE it: "go there". One device action, carrying the whole computed configuration — the
  // smallest span that covers the selection, then the snapped centre.
  const outside = regionOf(432.0e6, 432.4e6);
  const plan = freqZoomTarget(GRID, G, outside, true);
  assert.equal(plan.kind, "retune");
  if (plan.kind !== "retune") return;
  await applyFreqZoom(ctx, plan);
  assert.equal(calls.length, 2, "one retune: the covering rate, then the centre");
  assert.deepEqual(calls[0], { method: "POST", path: "/api/control/rate", body: { sample_rate_hz: 2_000_000 } });
  // The centre goes out exactly as planned: re-snapping an already-snapped centre is a no-op.
  assert.deepEqual(calls[1], { method: "POST", path: "/api/control/center", body: { center_hz: plan.centerHz } });
  // By value, not "a call happened": the centre is on the synthesiser grid and within a step of the
  // region's centre, and the window it opens contains the whole selection.
  const step = GRID.frequency!.center_step_hz!;
  const posted = (calls[1].body as { center_hz: number }).center_hz;
  assert.ok(Math.abs(posted - 432.2e6) <= step, `${posted} is more than one step off the region centre`);
  assert.ok(posted - 1e6 <= outside.lo && posted + 1e6 >= outside.hi, "the opened window must contain the selection");
  // The view it asked for is pending until the new header arrives, and no stale offer is left.
  assert.deepEqual(ctx.store.get().live.pendingView, { loHz: outside.lo, hiHz: outside.hi });
  assert.equal(ctx.store.get().live.retuneOffer, null);
  assert.match(ctx.store.get().toast.text, /Retuning hackrf:/);

  // The rate call is skipped when the window is already the right width: a rate change re-plumbs
  // the capture, so one is not made idly.
  calls.length = 0;
  ctx.store.set((s) => ({ device: { ...s.device, sampleRateHz: 2_000_000 } }));
  await applyFreqZoom(ctx, freqZoomTarget(GRID, G, regionOf(433.0e6, 433.4e6), true));
  assert.equal(calls.length, 1);
  assert.equal(calls[0].path, "/api/control/center");

  // A refusal reaches nothing at all, and says why.
  calls.length = 0;
  await applyFreqZoom(ctx, freqZoomTarget(GRID, G, { lo: 6.1e9, hi: 6.2e9 }, true));
  assert.deepEqual(calls, [], "a region nothing can capture must not be posted to the radio");
  assert.match(ctx.store.get().toast.text, /Outside what the front end can tune/);
});

test("T-392: the retune fires on RELEASE and there is no confirmation in front of it — but a PAN still only offers", () => {
  // The user's correction: *"REMOVE the 'Retune here' confirmation button — do not require
  // confirmation; the retune fires when the user RELEASES the cursor on a frequency-navigator
  // region select."* So `applyFreqZoom` — the one thing on this bar that can build a device action
  // — sits **after** the mount's `if (!done) return;` guard: nothing an in-flight drag does can
  // reach it, and nothing between the release and the request asks the user again.
  const src = readFileSync("src/app/centre/navigators.ts", "utf8");
  const freqMount = src.slice(src.indexOf("function mountFreqNav("), src.indexOf("function mountTimeNav("));
  const region = freqMount.indexOf("onRegion:");
  const guard = freqMount.indexOf("if (!done) return;", region);
  const fire = freqMount.indexOf("applyFreqZoom(", region);
  assert.ok(region > 0 && guard > region, "the region handler must return early while the drag is in flight");
  assert.ok(fire > guard, "the retune must fire only after the release, never on pointermove");
  // And the region path builds no offer: it clears one, it never creates one to be confirmed.
  const after = freqMount.slice(guard);
  assert.ok(!/setRetuneOffer\(\{/.test(after), "a region-select must not interpose a confirmation offer");

  // A **pan** is the other half of the decision, and it keeps the offer. A pan has no target
  // region to interpret — the user drags and stops — so firing a retune at the end of one would
  // make the radio move as a side effect of scrolling, which is exactly T-340's control. The
  // mechanism is not orphaned either: the waterfall's own edge pan (`viewHooks().edgeOffer`) and
  // the axis strip's button are the same offer.
  const panEnd = freqMount.slice(freqMount.indexOf("onPanEnd:"), region);
  assert.ok(panEnd.includes("setRetuneOffer"), "a pan past the edge still offers rather than commands");
  assert.ok(!panEnd.includes("applyDeviceAction") && !panEnd.includes("applyFreqZoom"),
    "…and it must not reach the radio itself");
});

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

test("T-340: a region dragged on the time navigator zooms the waterfall to that span, at a tier that errs coarser", () => {
  const t1 = 1_789_300_920;
  const ext = timeExtent(captureWindow({
    window: { t0_s: t1 - 600, t1_s: t1, span_s: 600, buffered: null },
  }))!;
  // The lower two thirds of the bar: time runs down, so fraction 1 is the live edge.
  const region = regionFromDrag(ext, 1 / 3, 1)!;
  near(region.lo, t1 - 400);
  near(region.hi, t1);

  const z = timeZoomTarget(GRID, region, 400)!;
  assert.equal(z.spanS, 400);
  assert.equal(z.tS, t1, "the zoom ends at the instant the drag ended");
  // 400 s over 400 rows wants 1 s cells, and the ladder has exactly that tier.
  assert.equal(z.tier!.t_cell_s, 1);
  // A deeper zoom than the ladder holds is answered coarser, never finer (T-334).
  const deep = timeZoomTarget(GRID, { lo: t1 - 10, hi: t1 }, 400)!;
  assert.equal(deep.tier!.t_cell_s, 1);
  assert.match(timeDetailText(deep), /history/);
  assert.match(timeDetailText(z), /7 min · history · 1 s cells/);
  assert.equal(timeZoomTarget(GRID, null, 400), null);

  // The zoom is a *view* change: it sets the review cursor's span, and the history request the
  // waterfall makes is that span exactly — not the rows-times-period fallback.
  const store = createStore(initialState());
  store.set(reviewAt(z.tS, z.spanS));
  const cur = store.get().time;
  assert.equal(cur.live, false);
  assert.equal(!cur.live && cur.spanS, 400);
  assert.deepEqual(historyWindow(z.tS, 512, 0.04, z.spanS), { t0: t1 - 400, t1 });
  // Control: with no span asked for, the window is still the rows' own span — never a constant.
  assert.deepEqual(historyWindow(z.tS, 512, 0.04, null), { t0: t1 - 20.48, t1 });
  assert.equal(sameCursor({ live: false, tS: t1, spanS: 400 }, { live: false, tS: t1, spanS: 20 }), false);
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

test("T-340 control: panning the frequency navigator reaches no device route, at any distance", () => {
  const { ctx, calls } = deviceSpyCtx();
  const ext = spectrumExtent(GRID.frequency)!;
  const v0 = ctx.store.get().live.view!;

  // Drags across the whole 6 GHz bar — the extreme case, thousands of times the tuned window and
  // hundreds of times the 5 % overflow that used to command a retune before T-343.
  for (const df of [0.001, 0.05, 0.5, 1, -0.001, -0.5, -1]) {
    const s = ctx.store.get();
    const r = freqPan(G, s.live.view!, ext, df);
    ctx.store.set(setLiveView(r.view));
    // What the mount does at the end of such a pan: record an offer. A store write, nothing more.
    if (Math.abs(r.overflowHz) > 0) {
      ctx.store.set(setRetuneOffer({
        centerHz: ax.panRetuneCenter(r.view, r.overflowHz),
        view: { loHz: r.view.loHz + r.overflowHz, hiHz: r.view.hiHz + r.overflowHz },
      }));
    }
  }
  assert.deepEqual(calls, [], "a navigator pan must never reach the control API");
  // …and it is not that the gesture did nothing: the view moved, and it clamped at the band edge.
  assert.notDeepEqual(ctx.store.get().live.view, v0);
  const full = ax.fullView(G);
  assert.ok(ctx.store.get().live.view!.loHz >= full.loHz - 1e-6);
  assert.ok(ctx.store.get().live.retuneOffer, "a pan past the edge offers a retune instead of performing one");
});

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

test("T-340: the one path from a navigator to the radio is an explicit device action, named as such", async () => {
  const { ctx, calls } = deviceSpyCtx();
  // What pressing the bar's offer button does — and the only thing on either bar that does it.
  await applyDeviceAction(ctx, retuneAction(432_200_000, "navigator", { loHz: 432e6, hiHz: 432.4e6 }));
  assert.equal(calls.length, 1);
  assert.equal(calls[0].path, "/api/control/center");
  // Snapped to the achievable grid before it is posted (T-341), and it names the radio it moved.
  const step = GRID.frequency!.center_step_hz!;
  const posted = (calls[0].body as { center_hz: number }).center_hz;
  near(posted / step - Math.round(posted / step), 0, 1e-6, );
  assert.ok(Math.abs(posted - 432_200_000) <= step, `${posted} is off the requested centre by more than one step`);
  assert.match(ctx.store.get().toast.text, /Retuning hackrf:/);

  // The navigator module itself names no device route: the source, not a convention.
  const src = readFileSync("src/app/centre/navigators.ts", "utf8");
  for (const r of ["/api/control/center", "/api/control/rate", "/api/control/gains"]) {
    assert.ok(!src.includes(r), `navigators.ts must not name ${r}`);
  }
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
// The mounts and their styles
// ---------------------------------------------------------------------------

test("T-340: the centre area mounts both navigators, and their CSS stays scoped to their slots", () => {
  assert.deepEqual(Object.keys(mounts).sort(), ["axis", "freqnav", "live", "timenav"]);
  assert.deepEqual(Object.keys(centreInitial()).sort(), ["live", "navGrid"]);
  const html = readFileSync("src/app/index.html", "utf8");
  // Parallel to their axes: the time bar beside the waterfall, the frequency bar below the axis.
  assert.match(html, /data-slot="timenav"[\s\S]*data-slot="live"/);
  assert.match(html, /data-slot="axis"[\s\S]*data-slot="freqnav"/);
  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  const selectors = [...css.matchAll(/([^{}@]+)\{[^{}]*\}/g)].map((m) => m[1].trim()).filter((s) => s && !s.startsWith("@"));
  for (const sel of selectors) for (const part of sel.split(",")) {
    assert.match(part.trim(), /^\.(specwf|axis|freqnav|timenav)\b/, `unscoped: ${part}`);
  }
  // The overview the time bar draws is upscaled, never smoothed — repeating a measured cell across
  // pixels is the honest direction (T-334); interpolating invents a value between them.
  assert.match(css, /\.timenav \.tn-overview \{[^}]*image-rendering: pixelated/);
});

/** The thin-client rule (CLAUDE.md): `ui/src` holds interaction and presentation only. A navigator
 * that reasoned about dB, occupancy or a front end's limits would be a second opinion about the
 * radio — the backend's job, and the failure mode T-341's split exists to prevent. */
test("T-340: no signal logic or RF constant lives in the navigator modules", () => {
  for (const f of ["src/navigators.ts", "src/app/centre/navigators.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["_db", "dbfs", "snr", "occupancy", "noise", "floor_db", "threshold_db", "e6", "e9", "MHz_"]) {
      assert.ok(!src.toLowerCase().includes(word.toLowerCase()), `${f} must not contain "${word}"`);
    }
    // No literal frequency: every Hz in these files came from the backend's grid.
    assert.ok(!/\b\d{6,}(\.\d+)?\b/.test(src), `${f} must not contain a hard-coded frequency`);
  }
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

test("T-367 property (b): the frequency navigator's content does not change when the time selection changes", () => {
  const { ctx } = deviceSpyCtx();
  /** Everything the frequency bar draws: its extent, its lit segments, and the view marker. */
  const content = () => {
    const s = ctx.store.get();
    const ext = spectrumExtent(s.navGrid.grid?.frequency ?? null);
    const v = s.live.view;
    return { ext, segs: litSegments(s.navGrid.windows, ext), marker: v ? placeOn(ext, v.loHz, v.hiHz) : null };
  };
  const before = content();
  const liveBefore = ctx.store.get().live, gridBefore = ctx.store.get().navGrid;
  assert.equal(before.segs.length, 1);

  // Scrub, zoom and return to live on the time bar — every gesture the vertical bar has.
  const t1 = 1_789_300_920;
  const ext: Range = timeExtent({ t0S: t1 - 600, t1S: t1, spanS: 600 })!;
  applyTimeTarget(ctx.store, timePanTarget(ext, { live: true, tS: 0, spanS: null }, -0.5));
  applyTimeTarget(ctx.store, timeWheelTarget(ext, { live: false, tS: t1 - 300, spanS: 30 }, 4));
  const region = regionFromDrag(ext, 0.2, 0.6)!;
  applyTimeTarget(ctx.store, { live: false, tS: region.hi, spanS: region.hi - region.lo });

  assert.equal(ctx.store.get().time.live, false, "the time cursor must actually have moved");
  assert.deepEqual(content(), before, "a time gesture must not change what the frequency bar draws");
  // Bit-identical, not merely equal: the frequency slices are the very same objects.
  assert.ok(Object.is(ctx.store.get().live, liveBefore), "state.live must be the same object after a time gesture");
  assert.ok(Object.is(ctx.store.get().navGrid, gridBefore), "state.navGrid must be the same object after a time gesture");
});

test("T-367 control: dragging the vertical bar changes only time — the frequency slice is bit-identical", () => {
  const { ctx, calls } = deviceSpyCtx();
  const t1 = 1_789_300_920;
  const ext: Range = timeExtent({ t0S: t1 - 600, t1S: t1, spanS: 600 })!;

  for (const df of [-0.001, -0.25, -1, 0.5, 1]) {
    const before = ctx.store.get();
    applyTimeTarget(ctx.store, timePanTarget(ext, { live: false, tS: t1 - 300, spanS: 20 }, df));
    const after = ctx.store.get();
    assert.ok(Object.is(after.live, before.live), `a pan of ${df} moved the frequency view`);
    assert.ok(Object.is(after.device, before.device), `a pan of ${df} moved the device state`);
  }
  // The gesture is not inert: panning back reaches the live edge, panning away leaves it.
  assert.equal(timePanTarget(ext, { live: false, tS: t1 - 300, spanS: 20 }, 1).live, true);
  const back = timePanTarget(ext, { live: true, tS: 0, spanS: null }, -0.5);
  assert.equal(back.live, false);
  near(!back.live ? back.tS : NaN, t1 - 300);
  // A pan keeps the span it is reviewing — it moves along time, it does not rescale it.
  const kept = timePanTarget(ext, { live: false, tS: t1 - 300, spanS: 20 }, -0.1);
  assert.equal(!kept.live && kept.spanS, 20);
  // Past the oldest edge it clamps, exactly as the frequency bar clamps at the band edge.
  const oldest = timePanTarget(ext, { live: false, tS: t1 - 300, spanS: 20 }, -10);
  near(!oldest.live ? oldest.tS : NaN, ext.lo);

  // Wheel: the span changes, the instant stays on the bar, and still no frequency moves.
  const beforeWheel = ctx.store.get();
  const z = timeWheelTarget(ext, { live: false, tS: t1 - 100, spanS: 40 }, 4);
  applyTimeTarget(ctx.store, z);
  assert.equal(!z.live && z.spanS, 10);
  assert.ok(Object.is(ctx.store.get().live, beforeWheel.live), "a time wheel zoom moved the frequency view");
  // Bounded by the capture window, never below the view floor, and a nonsense factor is inert.
  assert.equal((timeWheelTarget(ext, { live: false, tS: t1, spanS: 40 }, 1e-9) as { spanS: number }).spanS, 600);
  assert.ok((timeWheelTarget(ext, { live: false, tS: t1, spanS: 1e-3 }, 1e9) as { spanS: number }).spanS >= 1e-3);
  assert.deepEqual(timeWheelTarget(ext, { live: true, tS: 0, spanS: null }, 0), { live: true });

  assert.deepEqual(calls, [], "no time gesture may reach the control API");
});

test("T-367 control: dragging the horizontal bar changes only frequency — the time cursor is bit-identical", async () => {
  const { ctx, calls } = deviceSpyCtx();
  const ext = spectrumExtent(GRID.frequency)!;
  const fracOf = (hz: number) => (hz - ext.lo) / (ext.hi - ext.lo);
  // Start from a reviewed instant, so "the time cursor did not move" is a real thing to preserve.
  const t1 = 1_789_300_920;
  ctx.store.set(reviewAt(t1 - 42, 20));
  const timeBefore = ctx.store.get().time;

  // A pan across the bar.
  for (const df of [0.001, 0.2, 1, -1]) {
    const s = ctx.store.get();
    ctx.store.set(setLiveView(freqPan(G, s.live.view!, ext, df).view));
    assert.ok(Object.is(ctx.store.get().time, timeBefore), `a frequency pan of ${df} moved the time cursor`);
  }

  // A region dragged inside the tuned band: a view zoom.
  const inside = regionFromDrag(ext, fracOf(99.6e6), fracOf(100.4e6))!;
  await applyFreqZoom(ctx, freqZoomTarget(GRID, G, inside, true));
  assert.deepEqual(ctx.store.get().live.view, { loHz: inside.lo, hiHz: inside.hi });
  assert.ok(Object.is(ctx.store.get().time, timeBefore), "a frequency zoom moved the time cursor");
  assert.deepEqual(calls, [], "a pan, and a zoom inside the window, reach no control route");

  // A region outside it: T-392's retune — the one navigator action that commands the radio. It
  // still does not touch the time axis, which is the property under test here.
  const outside = regionFromDrag(ext, fracOf(432.0e6), fracOf(432.4e6))!;
  await applyFreqZoom(ctx, freqZoomTarget(GRID, G, outside, true));
  assert.ok(calls.some((c) => c.path === "/api/control/center"), "the frequency gesture must retune");
  assert.ok(Object.is(ctx.store.get().time, timeBefore), "retuning moved the time cursor");
  assert.deepEqual(ctx.store.get().time, { live: false, tS: t1 - 42, spanS: 20 });
  assert.ok(calls.every((c) => c.path.startsWith("/api/control/")), "and it reaches only control routes");
});

test("T-367 control: the source wires each bar to one axis — no frequency writer in the time mount, no time writer in the frequency mount", () => {
  const src = readFileSync("src/app/centre/navigators.ts", "utf8");
  const at = (name: string) => {
    const i = src.indexOf(`function ${name}(`);
    assert.ok(i > 0, `${name} not found`);
    return i;
  };
  // The file's two mounts, in source order: frequency first, then time, then the export.
  const [fStart, tStart] = [at("mountFreqNav"), at("mountTimeNav")];
  assert.ok(fStart < tStart);
  const freqMount = src.slice(fStart, tStart);
  const timeMount = src.slice(tStart, src.indexOf("export const navigatorMounts"));

  // The vertical bar never changes frequency.
  for (const w of ["setLiveView", "setRetuneOffer", "applyDeviceAction", "retuneAction", "applyFreqZoom", "setNavigation"]) {
    assert.ok(!timeMount.includes(w), `the TIME navigator must not write the frequency axis (${w})`);
  }
  // The horizontal bar never scrubs time — the exact bug the user named.
  for (const w of ["reviewAt", "goLive", "applyTimeTarget", "timePanTarget", "timeWheelTarget", "timeZoomTarget"]) {
    assert.ok(!freqMount.includes(w), `the FREQUENCY navigator must not scrub time (${w})`);
  }
  // …and it is not that the time mount does nothing: it applies time targets, and its picture is
  // asked for over a frequency range rather than over none.
  for (const w of ["applyTimeTarget", "timePanTarget", "timeWheelTarget", "timelineRequest", "currentSpan"]) {
    assert.ok(timeMount.includes(w), `the TIME navigator should use ${w}`);
  }
  // The regression itself: an unscoped `/api/timeline` request, which returns no grid at all.
  assert.ok(!/\/api\/timeline\?columns/.test(src), "the time navigator must not ask for an unscoped overview");
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

// ---------------------------------------------------------------------------
// T-395: Live moved onto the time navigator — the control, not the state
// ---------------------------------------------------------------------------
//
// The user's placement argument: *following the live edge vs holding a past window is a TIME-axis
// choice, so it belongs with the time navigator, consistent with "left bar = time".*
//
// The risk this suite exists for is the opposite of a layout bug. `time.live` is load-bearing:
// T-388's presence push subscribes **only** while it is true (a paused view opens no socket at
// all), T-379's view window is derived from the same cursor, and T-387's live-only notes change
// their text on it. So the assertions below are not that a button exists somewhere new — they are
// that **the relocation moved no state**: the same patch is dispatched, the frequency axis does not
// move with it, no device route is reached, and the presence stream's paused/live behaviour is
// bit-for-bit what T-388 landed.

test("T-395: the Live control dispatches the SAME action from the time bar, and moves nothing else", () => {
  const { ctx, calls } = deviceSpyCtx();
  ctx.store.set(reviewAt(1_600_000_300, 20));
  const liveBefore = ctx.store.get().live;

  goLiveFromNav(ctx.store);

  // The identical patch the Capture panel's pill produced, compared against a store driven by
  // `goLive` itself rather than against a hand-written literal.
  const ref = createStore(initialState());
  ref.set(reviewAt(1_600_000_300, 20));
  ref.set(goLive);
  assert.deepEqual(ctx.store.get().time, ref.get().time);
  assert.deepEqual(ctx.store.get().time, { live: true });

  // T-367's rule, restated for the new control: a time control writes the time cursor and nothing
  // else, so the frequency slice comes out the same object.
  assert.ok(Object.is(ctx.store.get().live, liveBefore), "the Live control must not move the frequency axis");
  assert.deepEqual(calls, [], "following the live edge is a view change, never a device action");
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

test("T-395: the presence push sees paused and live exactly as T-388 left it, through the relocated control", async () => {
  LiveSocket.open = [];
  (globalThis as Record<string, unknown>).WebSocket = LiveSocket;
  (globalThis as Record<string, unknown>).location = { protocol: "http:", host: "h" };
  (globalThis as Record<string, unknown>).window = globalThis;

  const store = createStore(initialState());
  store.set(reviewAt(900)); // held in the past, before anything mounts
  let gets = 0;
  const client = {
    get: async () => { gets++; return { streams: [{ stream_id: "presence", kind: "messages", remote_permitted: true }] }; },
  } as unknown as AppContext["client"];
  const stop = mountPresenceStream({ store, client, token: "t" } as AppContext);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(gets, 0, "paused: no stream is even discovered — T-388's rule, unchanged by the move");
  assert.equal(LiveSocket.open.length, 0, "and no socket is opened");

  // The relocated control, and nothing else, drives the transition.
  goLiveFromNav(store);
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(LiveSocket.open.length, 1, "pressing LIVE on the time bar subscribes, exactly as the Capture pill did");
  assert.ok(LiveSocket.open[0].url.includes("/ws/presence"));

  // …and pausing again still closes it: the pair is intact, not just the live half.
  store.set(reviewAt(900));
  assert.ok(LiveSocket.open[0].closed, "pausing still closes the socket");
  stop();
});

test("T-395: the control lives on the time navigator and no longer on the Capture panel", () => {
  const nav = readFileSync("src/app/centre/navigators.ts", "utf8");
  const timeMount = nav.slice(nav.indexOf("function mountTimeNav("), nav.indexOf("export const navigatorMounts"));
  assert.match(timeMount, /class: "tn-live"/, "the LIVE control is built by the TIME navigator");
  assert.match(timeMount, /goLiveFromNav\(store\)/, "and wired to the same time-axis action");

  // Gone from the Capture panel — the element, its handler and its style, not merely hidden.
  const cap = readFileSync("src/app/capture/index.ts", "utf8");
  assert.doesNotMatch(cap, /live-pill|livePill/, "the Capture panel no longer builds a LIVE control");
  const capCss = readFileSync("src/app/capture/capture.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.doesNotMatch(capCss, /live-pill/, "and its style went with it");

  // The move is a move, not a deletion: the style is on the time bar now, scoped to that slot.
  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.match(css, /\.timenav \.tn-live\s*\{/);
  assert.match(css, /\.timenav \.tn-live\.following\s*\{/, "it still shows which of the two states the view is in");
});

// ---------------------------------------------------------------------------
// T-393: what each bar is, and what it says under the cursor
// ---------------------------------------------------------------------------
//
// The user asked for three things: a static label naming each bar's axis; on the TIME bar the clock
// time at the cursor and the selected range during a drag; on the FREQUENCY bar the frequency at
// the cursor **and the SDR configuration selecting there would set**, snapped to the achievable
// grid. Minimal, hover/drag only.
//
// The one that has to be right is the clock. Four separate times in this UI a surface has rendered
// the browser's wall clock over capture data (T-379's 306,315 s-out window, T-384's three sites in
// `plots.ts`, T-389's live Confirmed query, and the trap T-387 had to prove it avoided). A time
// navigator that did it would be the worst of them, because a user reads a clock and believes it.

test("T-393: the time bar's readout is on the CAPTURE clock, and provably not the browser's", () => {
  // A capture window years away from wall-clock now — a replay, or the mock SDR: exactly the case
  // the previous four bugs were found in.
  const ext = timeExtent({ t0S: 1_600_000_000, t1S: 1_600_000_600, spanS: 600 })!;

  // 1600000000 is 2020-09-13T12:26:40Z; half way along a 600 s window is 300 s later.
  assert.equal(timeHoverText(ext, 0), "12:26:40Z");
  assert.equal(timeHoverText(ext, 0.5), "12:31:40Z");
  assert.equal(timeHoverText(ext, 1), "12:36:40Z");

  // THE CONTROL: the same cursor position, formatted from the browser's clock instead. The readout
  // is not that — and cannot become it, because `Date.now()` is not an input to the function.
  const browser = clockText(Date.now() / 1000);
  assert.notEqual(timeHoverText(ext, 0.5), browser, "the readout is the browser's wall clock");
  assert.notEqual(timeHoverText(ext, 1), browser, "even the live edge is the CAPTURE clock's edge");

  // …and the same position on a *different* capture window gives a different answer, so the mapping
  // is genuinely the window's and not a constant that happened to match.
  const other = timeExtent({ t0S: 1_600_003_600, t1S: 1_600_004_200, spanS: 600 })!;
  assert.notEqual(timeHoverText(other, 0.5), timeHoverText(ext, 0.5));

  // No window answered yet: nothing is claimed, rather than a wall-clock time standing in.
  assert.equal(timeHoverText(null, 0.5), "");
});

test("T-393: dragging the time bar reads out the selected RANGE and how long it is", () => {
  const ext = timeExtent({ t0S: 1_600_000_000, t1S: 1_600_000_600, spanS: 600 })!;
  // A drag from a third of the way down to two thirds: 200 s in, 400 s in.
  // `durationText` is the band's own wording, reused rather than re-spelled here (200 s → "3 min").
  assert.equal(timeDragText(ext, 1 / 3, 2 / 3), "12:30:00Z–12:33:20Z · 3 min");
  assert.equal(timeDragText(ext, 0.5, 0.6), "12:31:40Z–12:32:40Z · 60 s");
  // Dragged upward (newest to oldest) it names the same range: a range has no direction.
  assert.equal(timeDragText(ext, 2 / 3, 1 / 3), timeDragText(ext, 1 / 3, 2 / 3));
  // A press with no width is not a range — it falls back to the instant under the cursor.
  assert.equal(timeDragText(ext, 0.5, 0.5), timeHoverText(ext, 0.5));
  // The range text, too, is on the capture clock.
  assert.equal(clockRangeText(1_600_000_600, 1_600_000_000), "12:26:40Z–12:36:40Z");
});

test("T-393: the frequency bar says where the cursor is AND the config selecting there would set — snapped", () => {
  const ext = spectrumExtent(GRID.frequency)!;
  const at = (hz: number) => (hz - ext.lo) / spanOf(ext);
  // The view is 1 MHz wide (99.5–100.5), which is NARROWER than any span this front end can open:
  // `spans_hz.min` is 2 MHz. The readout must therefore say 2.000 MHz, not the 1 MHz asked for —
  // that is the T-341 snap, reused rather than re-derived here.
  const text = freqHoverText(GRID, ext, at(100e6), 1e6);
  assert.match(text, /^100\.\d+ MHz · centre /, "where the cursor is comes first");
  assert.match(text, /span 2\.000 MHz \(sample rate\)/, "the achievable span, and what it is");
  assert.match(text, /· live IQ$/, "and the detail claim that comes with it");
  // The snapped centre is `snapState`'s, not the raw pointer frequency.
  const snapped = snapState(GRID, valueAt(ext, at(100e6)), 1e6);
  assert.ok(snapped.centerHz !== null && text.includes(ax.fmtMHz(snapped.centerHz, 1e6 / 100)), text);

  // A front end that cannot state its tuning step snaps no centre — and the readout says so rather
  // than implying the radio can sit exactly where the user pointed (navigation.ts, rule 1).
  const vague: NavigationGrid = { ...GRID, frequency: { ...GRID.frequency!, center_step: "unknown", center_step_hz: null } };
  assert.match(freqHoverText(vague, ext, at(100e6), 1e6), /centre not on any stated grid/);

  // Nothing tuned: where the cursor is, and no invented window around it.
  assert.equal(freqHoverText(GRID, ext, at(100e6), null).includes("centre"), false);
  assert.equal(freqHoverText(GRID, null, 0.5, 1e6), "");
});

test("T-393: the frequency bar's drag readout describes what selecting ACTUALLY does in this build", () => {
  const ext = spectrumExtent(GRID.frequency)!;
  const fracOf = (hz: number) => (hz - ext.lo) / spanOf(ext);

  // Inside the tuned window: a view zoom. The radio does not move, and the readout does not say it
  // would — the failure mode being a line that promises behaviour the code has not got.
  const inside = regionFromDrag(ext, fracOf(99.6e6), fracOf(100.4e6))!;
  const insideText = freqSelectText(GRID, G, inside, true);
  assert.match(insideText, /zooms the view, the radio stays put/);
  assert.doesNotMatch(insideText, /Retune/);

  // Outside it: T-392 made this gesture COMMAND the radio on release, so the readout says it
  // retunes. (Before T-392 it left an offer to press, and this line said "offers" — the control
  // below is what caught that, and it is kept here inverted rather than deleted.)
  const outside = regionFromDrag(ext, fracOf(432.0e6), fracOf(432.4e6))!;
  const outsideText = freqSelectText(GRID, G, outside, true);
  assert.match(outsideText, /retunes the radio on release: centre 432\.\d+ MHz · span 2\.000 MHz \(sample rate\) · live IQ/);
  assert.doesNotMatch(outsideText, /offer/, "region-select no longer offers anything — it acts");
  // The config in the line is `retunePlan`'s own, not a second opinion re-derived beside it.
  const planned = freqZoomTarget(GRID, G, outside, true);
  assert.equal(planned.kind, "retune");
  if (planned.kind === "retune") {
    assert.ok(outsideText.includes(ax.fmtMHz(planned.centerHz, Math.max(1, planned.spanHz / 100))), outsideText);
    assert.ok(outsideText.includes(ax.fmtBandwidth(planned.spanHz)), outsideText);
  }

  // Nothing can capture it: the readout is the refusal's OWN words, so the line and the action
  // cannot disagree about why. Three real refusals, all reached through the same branch.
  const tooWide = regionFromDrag(ext, fracOf(1.0e9), fracOf(2.0e9))!;     // wider than one window
  // A front end with a GAP in its coverage, so "centre outside every band" is a region a user can
  // really drag on this bar rather than a value only a test could construct.
  const SPLIT: NavigationGrid = { ...GRID, frequency: { ...GRID.frequency!, ranges_hz: [[1e6, 100e6], [400e6, 6e9]] } };
  const inGap = regionFromDrag(ext, fracOf(200e6), fracOf(210e6))!;       // centre in the uncovered gap
  for (const [why, grid, region, isLive] of [
    ["span_too_wide", GRID, tooWide, true],
    ["center_out_of_range", SPLIT, inGap, true],
    ["not_live", GRID, outside, false],
  ] as const) {
    const t = freqZoomTarget(grid, G, region, isLive);
    assert.equal(t.kind, "error", why);
    if (t.kind !== "error") continue;
    assert.equal(t.reason, why);
    const text = freqSelectText(grid, G, region, isLive);
    assert.match(text, /cannot capture this — /);
    assert.ok(text.endsWith(t.text), `the readout must use the refusal's own words: ${text}`);
  }

  // THE CONTROL that keeps the sentence and the gesture from drifting apart, in T-392's shape:
  // whichever branch `freqZoomTarget` takes is the branch the text describes, for every region,
  // grid and liveness either of them sees. This is the assertion that failed loudly when T-392
  // landed and turned the offer into a retune.
  const cases = [GRID, SPLIT].flatMap((grid) =>
    [inside, outside, tooWide, inGap, regionFromDrag(ext, fracOf(5.9e9), fracOf(5.95e9))!]
      .map((r) => [grid, r] as const));
  for (const [grid, r] of cases) for (const isLive of [true, false]) {
    const kind = freqZoomTarget(grid, G, r, isLive).kind;
    const text = freqSelectText(grid, G, r, isLive);
    assert.equal(/zooms the view/.test(text), kind === "view", `view: ${kind} — ${text}`);
    assert.equal(/retunes the radio on release/.test(text), kind === "retune", `retune: ${kind} — ${text}`);
    assert.equal(/cannot capture this/.test(text), kind === "error", `error: ${kind} — ${text}`);
    assert.equal(text === "", kind === "none", `none: ${kind} — ${text}`);
  }
  assert.equal(freqSelectText(GRID, G, null, true), "");
});

test("T-393: each bar carries a static label for its own axis, and a readout shown only on hover or drag", () => {
  const nav = readFileSync("src/app/centre/navigators.ts", "utf8");
  const freqMount = nav.slice(nav.indexOf("function mountFreqNav("), nav.indexOf("function mountTimeNav("));
  const timeMount = nav.slice(nav.indexOf("function mountTimeNav("), nav.indexOf("export const navigatorMounts"));

  // (1) the static labels, each naming the axis its own bar controls.
  assert.match(timeMount, /class: "tn-axis-label" \}, "TIME"/);
  assert.match(freqMount, /class: "fn-axis-label" \}, "FREQ"/);

  // (2)/(3) a readout per bar, hidden at rest and revealed by the pointer — not a permanent tick row.
  for (const [name, mount] of [["time", timeMount], ["frequency", freqMount]] as const) {
    assert.match(mount, /readout\.hidden = true/, `${name}: the readout rests hidden`);
    assert.match(mount, /addEventListener\("pointermove"/, `${name}: it follows the cursor`);
    assert.match(mount, /addEventListener\("pointerleave", \(\) => \{ dragText = null; readout\.hidden = true; \}\)/,
      `${name}: and goes away when the pointer does`);
  }
  // Each bar's readout moves along its OWN axis: the time line travels vertically, the frequency
  // line horizontally. Wiring either to the other's axis is the T-367 class of bug, in miniature.
  assert.match(timeMount, /readout\.style\.top =/);
  assert.ok(!/readout\.style\.left =/.test(timeMount), "the time readout must not travel horizontally");
  assert.match(freqMount, /readout\.style\.left =/);
  assert.ok(!/readout\.style\.top =/.test(freqMount), "the frequency readout must not travel vertically");

  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  for (const sel of [".timenav .tn-axis-label", ".timenav .tn-readout", ".freqnav .fn-axis-label", ".freqnav .fn-readout"]) {
    assert.ok(css.includes(`${sel} {`), `missing ${sel}`);
  }
  // Readouts are labels, never pointer targets: one over the track must not eat the drag under it.
  assert.match(css, /\.timenav \.tn-readout \{[^}]*pointer-events: none/);
  assert.match(css, /\.freqnav \.fn-readout \{[^}]*pointer-events: none/);
});

test("T-393: no clock of the browser's own reaches either navigator module", () => {
  // The structural half of the capture-clock property: the functions above cannot start reading
  // wall-clock time in a later edit without this failing. (T-388 pins the same thing on the
  // presence modules, for the same reason.)
  for (const f of ["src/navigators.ts", "src/app/centre/navigators.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset"]) {
      assert.ok(!src.includes(word), `${f} must not contain "${word}"`);
    }
  }
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

test("T-405: the frequency bar dims what is NOT being captured, instead of drawing a view box", () => {
  const ext: Range = { lo: 0, hi: 100 };
  const win = (lo: number, hi: number) => ({ center_hz: (lo + hi) / 2, span_hz: hi - lo, f_lo_hz: lo, f_hi_hz: hi });
  const one = activeWindows({ windows: [win(40, 60)] });
  const dim = dimSegments(one, ext);
  // Two dim spans, one either side; the window itself is left alone. That region at full strength
  // IS the tuned window — there is no second rectangle saying so.
  assert.equal(dim.length, 2);
  assert.deepEqual(dim.map((d) => [Math.round(d.startPct), Math.round(d.sizePct)]), [[0, 40], [60, 40]]);
  // Exactly the complement of the lit segments, from the same reported list.
  const lit = litSegments(one, ext);
  assert.equal(lit.length, 1);
  assert.ok(Math.abs(dim[0].sizePct + lit[0].sizePct + dim[1].sizePct - 100) < 1e-9);

  // Nothing reported: the whole bar dims. That is the honest picture of a server capturing nowhere
  // — never an assumption that there is one window somewhere.
  assert.deepEqual(dimSegments(activeWindows(null), ext), [{ startPct: 0, sizePct: 100 }]);
  assert.deepEqual(dimSegments(activeWindows({ windows: [] }), ext), [{ startPct: 0, sizePct: 100 }]);
  // Two front ends on adjacent ranges leave no seam between them.
  const two = activeWindows({ windows: [win(0, 50), win(50, 100)] });
  assert.deepEqual(dimSegments(two, ext), []);
  assert.deepEqual(dimSegments(one, null), []);

  // And the box is gone from the markup and the stylesheet, not merely hidden.
  const src = readFileSync("src/app/centre/navigators.ts", "utf8")
    .replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  assert.ok(!src.includes("fn-view"), "the persistent view box must be removed, not hidden");
  assert.ok(src.includes("fn-draft"), "the transient drag draft stays — drag-to-retune is unchanged");
  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.ok(!css.includes(".fn-view"), "no style may remain for a box nothing draws");
  assert.match(css, /\.freqnav \.fn-dim \{/);
});

test("T-397: the two strips and the waterfall share ONE colour ramp, and it reaches yellow, red and white", () => {
  // The symptom the user reported: the strips were cyan whatever the value, because their ramp ran
  // dark-teal → cyan and stopped. Cyan is the stop at x ≈ 0.45 of the real ramp, so a strip that
  // only ever reached the middle of it IS that teal band.
  const [, cyanG, cyanB] = cmapBytes(0.45);
  assert.ok(cyanG > 150 && cyanB > 180, `0.45 should be cyan: ${cmapBytes(0.45)}`);
  const [yr, yg, yb] = cmapBytes(0.7);
  assert.ok(yr > 200 && yg > 180 && yb < 80, `0.7 should be yellow: ${cmapBytes(0.7)}`);
  const [rr, rg, rb] = cmapBytes(0.9);
  assert.ok(rr > 200 && rg < 90 && rb < 60, `0.9 should be red: ${cmapBytes(0.9)}`);
  assert.deepEqual(cmapBytes(1), [255, 255, 255], "the top of the ramp is white");
  assert.deepEqual(cmapBytes(0), [0, 0, 10], "and the bottom is near-black");
  // Clamped, and a non-finite input reads as the bottom rather than wrapping.
  assert.deepEqual(cmapBytes(9), cmapBytes(1));
  assert.deepEqual(cmapBytes(-9), cmapBytes(0));
  assert.deepEqual(cmapBytes(NaN), cmapBytes(0));

  // The GLSL the waterfall compiles is GENERATED from the same stops, so the shader and the canvas
  // cannot drift: every stop's colour appears in it, in order.
  for (const [, c] of CMAP_STOPS) {
    const vec = `vec3(${c.map((v) => (Number.isInteger(v) ? v.toFixed(1) : String(v))).join(",")})`;
    assert.ok(CMAP_GLSL.includes(vec), `${vec} missing from the generated shader ramp`);
  }

  // THE CONTROL on duplication: the ramp exists in exactly one module under src/. A third copy — the
  // quickest fix, and the reason two renderings disagreed in the first place — fails here.
  const files = [
    "src/waterfall.ts", "src/app/centre/navigators.ts", "src/app/centre/live-spectrum.ts",
    "src/app/centre/review-render.ts", "src/axis.ts", "src/timebox.ts",
  ];
  for (const f of files) {
    const src = readFileSync(f, "utf8");
    assert.ok(!/vec3 cmap\(float/.test(src), `${f} must not define a second ramp`);
    assert.ok(!src.includes("0.05,0.1,0.55"), `${f} must not carry the ramp's stops`);
  }
  // The waterfall takes the generated one, and the strips take the byte form of the same stops.
  assert.match(readFileSync("src/waterfall.ts", "utf8"), /CMAP\s*=\s*CMAP_GLSL/);
  const nav = readFileSync("src/app/centre/navigators.ts", "utf8");
  assert.match(nav, /cmapBytes\(shade\)/);
  // ...and no longer the hand-rolled teal arithmetic it used to paint both strips with.
  assert.ok(!/120 \+ 110 \*/.test(nav), "the cyan-only ramp must be gone from the strips");
});

test("T-397: the bottom bar asks for a MINI-WATERFALL — time rows and frequency cells — over the same window as the time bar", () => {
  // The guard class CLAUDE.md names: assert the request the client BUILDS, not only what it renders
  // (T-367's bug was a perfectly-rendered answer to the wrong question).
  //
  // `/api/timeline`'s `columns` is its TIME axis and `rows` its FREQUENCY axis, so a mini-waterfall
  // R rows tall and N cells wide is `columns=R&rows=N`. No second axis had to be added anywhere:
  // `RegionHistory::overview` already folds both, which is the projection docs/16 §3 asks the
  // survey bar to be.
  const band = { loHz: 88e6, hiHz: 108e6 };
  const req = timelineRequest(band, 64, 380);
  assert.ok(req.includes("columns=64") && req.includes("rows=380"), req);
  // The frequency cells of both halves must match, or column f of one is not column f of the other.
  const cov = coverageRequest({ lo: band.loHz, hi: band.hiHz }, 380, null)!;
  assert.ok(cov.includes("cells=380"), cov);

  const src = readFileSync("src/app/centre/navigators.ts", "utf8");
  // Both bars size their request from their own element, not from a constant.
  assert.match(src, /stripCells\(track\.clientWidth/);
  assert.match(src, /stripCells\(track\.clientHeight/);
  assert.ok(!src.includes("const TIME_COLUMNS"), "the fixed 160 x 6 grid must be gone");
  assert.ok(!src.includes("const TIME_ROWS"), "the fixed 160 x 6 grid must be gone");
  // And the bottom strip's canvas is nt rows tall, not one row stretched by CSS.
  assert.match(src, /strip\.height = nt/);
  assert.ok(
    !/strip\.width = survey\.length;\s*\n\s*strip\.height = 1;/.test(src),
    "a single row stretched to the bar's height is the bug",
  );
});
