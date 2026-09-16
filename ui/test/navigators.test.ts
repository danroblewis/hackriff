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
//     restated for the surface the user asked to be able to set the centre from.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as ax from "../src/axis";
import { snapState, type NavigationGrid } from "../src/navigation";
import {
  activeWindows, clampInto, litSegments, panWithin, placeOn, regionFromDrag, spectrumExtent,
  timeExtent, valueAt, zoomWithin,
} from "../src/navigators";
import { captureWindow } from "../src/app/capture/timeline";
import { freqPan, freqZoomTarget, timeDetailText, timeZoomTarget } from "../src/app/centre/navigators";
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
