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
  activeWindows, bandKey, clampInto, coverageRequest, defaultViewport, litSegments, panWithin,
  placeOn, regionFromDrag, sameBand, spanOf, spectrumExtent, surveyCells, surveyViewport,
  timeExtent, timelineRequest, unobservedCount, valueAt, zoomWithin,
  type CoverageCell, type CoverageResponse, type Range,
} from "../src/navigators";
import { captureWindow, currentSpan } from "../src/app/capture/timeline";
import {
  applyFreqZoom, applyTimeTarget, freqPan, freqZoomTarget, timeDetailText, timePanTarget,
  timeWheelTarget, timeZoomTarget,
} from "../src/app/centre/navigators";
import { mounts } from "../src/app/centre";
import { historyWindow, sameCursor } from "../src/app/centre/review-render";
import { centreInitial, setNavigation } from "../src/app/centre/slice";
import { applyDeviceAction, retuneAction, setLiveView, setRetuneOffer } from "../src/app/centre/view";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState, reviewAt } from "../src/app/state";

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

  // A drag wider than one capture window is **survey overview**, and says so before it happens.
  const wide = regionFromDrag(ext, fracOf(80e6), fracOf(200e6))!;
  const w = freqZoomTarget(GRID, G, wide, true);
  assert.equal(w.kind, "offer");
  if (w.kind !== "offer") return;
  assert.equal(w.source, "survey-overview");
});

test("T-340 control: a region outside the tuned band OFFERS a retune on the achievable grid — it does not zoom", () => {
  const ext = spectrumExtent(GRID.frequency)!;
  const fracOf = (hz: number) => (hz - ext.lo) / (ext.hi - ext.lo);
  // A region a long way from the tuned 100 MHz: the waterfall does not hold this IQ.
  const region = regionFromDrag(ext, fracOf(432.0e6), fracOf(432.4e6))!;
  const z = freqZoomTarget(GRID, G, region, true);
  assert.equal(z.kind, "offer", "showing a region outside the window needs the radio moved");
  if (z.kind !== "offer") return;
  // The offered centre is on the front end's own synthesiser grid (T-341), not a rounded hertz.
  const step = GRID.frequency!.center_step_hz!;
  near(z.centerHz / step - Math.round(z.centerHz / step), 0, 1e-6);
  near(z.centerHz, snapState(GRID, 432.2e6, 400e3).centerHz!, 1e-3);
  assert.deepEqual(z.view, { loHz: region.lo, hiHz: region.hi });

  // With an unknown tuning step nothing snaps: the region's own centre is carried, and the offer
  // makes no claim that the device can sit exactly there (`snapState` returns null).
  const unknown: NavigationGrid = {
    ...GRID, frequency: { ...GRID.frequency!, center_step: "unknown", center_step_hz: null },
  };
  const u = freqZoomTarget(unknown, G, region, true);
  near(u.kind === "offer" ? u.centerHz : NaN, 432.2e6, 1);
  assert.equal(snapState(unknown, 432.2e6, 400e3).centerHz, null);

  // A replay has no front end to offer: nothing happens rather than a dead button.
  assert.equal(freqZoomTarget(GRID, null, region, false).kind, "none");
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

test("T-367 control: dragging the horizontal bar changes only frequency — the time cursor is bit-identical", () => {
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
  applyFreqZoom(ctx.store, freqZoomTarget(GRID, G, inside, true));
  assert.deepEqual(ctx.store.get().live.view, { loHz: inside.lo, hiHz: inside.hi });
  assert.ok(Object.is(ctx.store.get().time, timeBefore), "a frequency zoom moved the time cursor");

  // A region outside it: an offer. Still no time movement, and still no device reached.
  const outside = regionFromDrag(ext, fracOf(432.0e6), fracOf(432.4e6))!;
  applyFreqZoom(ctx.store, freqZoomTarget(GRID, G, outside, true));
  assert.ok(ctx.store.get().live.retuneOffer, "the frequency gesture must still offer the retune");
  assert.ok(Object.is(ctx.store.get().time, timeBefore), "offering a retune moved the time cursor");
  assert.deepEqual(ctx.store.get().time, { live: false, tS: t1 - 42, spanS: 20 });
  assert.deepEqual(calls, [], "no frequency gesture may reach the control API by itself");
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
  // It never asks for a time range: this bar is the frequency axis (T-367).
  assert.ok(!path.includes("t0="), "the frequency navigator never scrubs time");
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
