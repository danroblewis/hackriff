// T-981: the `frontend` layer — a front-end overload (the ADC clipped and the whole tuned window
// lifted) is marked as the RADIO'S energy, never drawn as a signal.
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **The request the client builds** is one `GET /api/frontend/events` over the union of the
//     time spans of the panes showing the layer — and NO request when no pane shows it. Nothing
//     here judges anything: the events are the backend's.
//  2. **The mark sits at the event's capture time**, placed by the tiles' own `toClip`, across the
//     event's window clipped to the pane; a sub-pixel (one-row) event is still visible.
//  3. **A distinct mark, never a signal's**: its own `frontend-event` kind and ink, a hatch between
//     two edges — a stroke, never a wash — never a `signal-box`.
//  4. **Registered and on by default** in the overlay plane, so the stripe never reads as signal.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { defaultPaneLayers, isLayerVisible, layerDef } from "../src/surface/layers";
import { inkFraction } from "../src/surface/minimap";
import { toClip, type PaneRect } from "../src/surface/surface";
import {
  FRONTEND_MARK, FRONTEND_MIN_PX, frontEndKeyEntries, frontEndQuads, frontEndRequest, parseFrontEndEvents,
  type FrontEndEvent,
} from "../src/surface/frontend";
import { LINK_INK } from "../src/surface/artifacts";
import { PATH_MARK } from "../src/surface/paths";
import { TUNE_INKS } from "../src/surface/tunepath";

const S = 1e9;
const T0 = 1_789_300_802 * S;
const PANE = { f0Hz: 914.5e6, f1Hz: 916.5e6, t0Ns: T0 - 20 * S, t1Ns: T0 + 1 * S };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

/** One two-row event over a 2 MHz window centred on 915 MHz — the wire shape docs/api.md shows. */
const WIRE = {
  window: { t0: 0, t1: 4e9 },
  events: [{
    kind: "clip", t0: T0 / S - 0.026, t1: T0 / S + 0.055, t0_ns: T0 - 26_000_000, t1_ns: T0 + 55_000_000,
    device_id: "hackrf:a", center_hz: 915e6, sample_rate_hz: 2e6, f_lo_hz: 914e6, f_hi_hz: 916e6,
    rows: 2, clipped_samples: 75295, samples: 161792, clip_fraction_max: 0.59, adc_peak_max: 1, step_db_max: 28.4,
  }],
  total: 1, limit: 256, truncated: false, log: { capacity: 1024, retained: 1, oldest_s: 0, evicted: 0 },
};

test("frontend: one GET /api/frontend/events over the union of the showing panes' time spans", () => {
  const a = { f0Hz: 914e6, f1Hz: 916e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
  const b = { f0Hz: 99e6, f1Hz: 101e6, t0Ns: T0 - 60 * S, t1Ns: T0 - 30 * S };
  assert.equal(frontEndRequest([a, b]), `/api/frontend/events?t0=${(T0 - 60 * S) / S}&t1=${T0 / S}`);
  assert.equal(frontEndRequest([]), null, "no pane shows the layer: no request at all");
  assert.equal(frontEndRequest([{ ...a, t0Ns: -20 * S, t1Ns: 0 }]), null, "nor before the first row");
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /frontEndRequest\(pv\.view\.panes\.list\(\)\s*\.filter\(\(x\) => isLayerVisible\(layersFor\(x\.id\), "frontend"\)\)/);
  const fns = /overlayFns: Partial<Record<LayerId, OverlayLayerFn>> = \{([\s\S]*?)\}/.exec(src);
  assert.ok(fns, "the overlay renderer table");
  assert.match(fns[1], /\bfrontend: frontEndQuadsFn\b/);
});

test("frontend: the wire answer parses; anything malformed is dropped, never drawn", () => {
  const got = parseFrontEndEvents(WIRE);
  assert.equal(got.length, 1);
  assert.deepEqual(
    { device: got[0].device, f0: got[0].f0Hz, f1: got[0].f1Hz, rows: got[0].rows },
    { device: "hackrf:a", f0: 914e6, f1: 916e6, rows: 2 },
  );
  assert.ok(Math.abs(got[0].t0Ns - (T0 - 26_000_000)) < 1e3);
  for (const junk of [null, {}, { events: "x" }, { events: [{ t0: 1 }] }, { events: [{ t0: 2, t1: 1, f_lo_hz: 1, f_hi_hz: 2 }] }]) {
    assert.deepEqual(parseFrontEndEvents(junk), [], JSON.stringify(junk));
  }
});

test("frontend: the band sits at the event's capture time, across its window clipped to the pane", () => {
  const [ev] = parseFrontEndEvents(WIRE);
  const q = frontEndQuads([ev], PANE, RECT);
  const fill = q.filter((x) => x.part === "fill");
  const edges = q.filter((x) => x.part === "edge");
  assert.equal(fill.length, 1);
  assert.equal(edges.length, 2);
  const want = toClip({ f0Hz: ev.f0Hz, f1Hz: ev.f1Hz, t0Ns: ev.t0Ns, t1Ns: ev.t1Ns }, PANE);
  const [x0, y0, x1, y1] = fill[0].clip;
  // Frequency: the event's window, clipped to the pane (the pane starts above 914 MHz).
  assert.equal(x0, -1);
  assert.ok(Math.abs(x1 - want[2]) < 1e-12);
  // Time: centred on the event's own rows, and at least FRONTEND_MIN_PX tall.
  const c = (want[1] + want[3]) / 2;
  assert.ok(Math.abs((y0 + y1) / 2 - c) < 1e-12, "centred on the event's capture time");
  assert.ok((y1 - y0) * RECT.h / 2 >= FRONTEND_MIN_PX - 1e-9, "a one-row event is still visible");
  // Off the pane in time or frequency: nothing.
  assert.deepEqual(frontEndQuads([ev], { ...PANE, t0Ns: T0 + 10 * S, t1Ns: T0 + 20 * S }, RECT), []);
  assert.deepEqual(frontEndQuads([ev], { ...PANE, f0Hz: 100e6, f1Hz: 101e6 }, RECT), []);
});

test("frontend: a distinct mark — its own kind and ink, a hatch between edges, never a signal box", () => {
  const q = frontEndQuads(parseFrontEndEvents(WIRE), PANE, RECT);
  for (const x of q) {
    assert.equal(x.kind, "frontend-event", "never a signal-box");
    assert.deepEqual(x.rgba, FRONTEND_MARK);
  }
  const fill = q.find((x) => x.part === "fill")!;
  assert.equal(fill.pattern?.mode, "hatch");
  assert.ok(inkFraction(fill) < 0.5, "a stroke, never a wash: the energy under it stays readable");
  const others = [PATH_MARK, ...TUNE_INKS, ...Object.values(LINK_INK)];
  for (const ink of others) {
    const d = Math.hypot(ink[0] - FRONTEND_MARK[0], ink[1] - FRONTEND_MARK[1], ink[2] - FRONTEND_MARK[2]);
    assert.ok(d > 0.2, `the front-end ink is off ${ink}`);
  }
  assert.match(frontEndKeyEntries(parseFrontEndEvents(WIRE))[0].note, /not a signal/);
  assert.match(frontEndKeyEntries([] as FrontEndEvent[])[0].note, /No front-end event in view/);
});

test("frontend: registered in the overlay plane and on by default", () => {
  const d = layerDef("frontend");
  assert.ok(d);
  assert.equal(d.plane, "overlay");
  assert.equal(d.visibleByDefault, true);
  assert.equal(isLayerVisible(defaultPaneLayers("p1"), "frontend"), true);
});
