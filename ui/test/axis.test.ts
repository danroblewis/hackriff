// T-045 axis mapping: bins → Hz → screen → pointer, against the geometry and peak bins the real
// pipeline produced (spectrum_axis.golden.json, written by tests/e2e/tests/spectrum_axis.rs).
import { test } from "node:test";
import assert from "node:assert/strict";
import * as ax from "../src/axis";
import golden from "./spectrum_axis.golden.json";

interface GoldenCase { center_hz: number; bandwidth_hz: number; fft_size: number; signal_hz: number; peak_bin: number }

const near = (a: number, b: number, tol: number, msg = "") => assert.ok(Math.abs(a - b) <= tol, `${msg}: |${a} − ${b}| > ${tol}`);
const FM = { centerHz: 100.8e6, bandwidthHz: 2.4e6, bins: 4096 };

test("bins are DC-centred and ascending (hk-dsp order, StreamHeader::spectrum_bin_hz)", () => {
  const df = ax.binWidthHz(FM);
  assert.equal(df, 585.9375);
  assert.equal(ax.binHz(FM, 2048), 100.8e6);
  assert.equal(ax.binHz(FM, 0), 99.6e6);
  assert.equal(ax.binHz(FM, 4095), 102e6 - df);
  assert.equal(ax.hzToBin(FM, 100.8e6), 2048);
  assert.equal(ax.hzToBin(FM, 101.3e6), 2901);
  const full = ax.fullView(FM);
  assert.equal(full.loHz, 99.6e6 - df / 2);
  assert.equal(full.hiHz, 102e6 - df / 2);
  near(ax.fracToHz(full, 2048.5 / 4096), 100.8e6, 1e-3, "DC bin centre on screen");
  const odd = { centerHz: 0, bandwidthHz: 5, bins: 5 };
  assert.deepEqual([0, 1, 2, 3, 4].map((i) => ax.binHz(odd, i)), [-2, -1, 0, 1, 2]);
  assert.equal(ax.geometryOf({ center_hz: 1, bandwidth_hz: 0, fft_size: 8 }), null);
  assert.deepEqual(ax.geometryOf({ center_hz: 100.8e6, bandwidth_hz: 2.4e6, fft_size: 4096 }), FM);
});

test("pointer position maps through the element box and snaps to the bin under it", () => {
  const full = ax.fullView(FM), df = ax.binWidthHz(FM), rect = { left: 22, width: 1236 };
  near(ax.fracToHz(full, ax.pointerFrac(22 + 618, rect)), 100.8e6, df, "canvas centre is the tuned centre, not 100.4324 MHz");
  // The middle pixel is half a bin below DC (even N); the readout reports the DC bin itself.
  for (const g of [FM, { ...FM, bins: 1024 }]) {
    const f = ax.fullView(g);
    assert.equal(ax.snapHz(g, ax.fracToHz(f, 0.5)), 100.8e6, `${g.bins} bins`);
    assert.equal(ax.fmtMHz(ax.snapHz(g, ax.fracToHz(f, 0.5)), ax.binWidthHz(g)), g.bins === 1024 ? "100.800" : "100.8000");
  }
  assert.equal(ax.snapHz(FM, 1e3), 99.6e6, "clamped to bin 0");
  assert.equal(ax.snapHz(FM, 1e12), ax.binHz(FM, 4095), "clamped to the last bin");
  assert.equal(ax.pointerFrac(0, rect), 0);
  assert.equal(ax.pointerFrac(5000, rect), 1);
  assert.equal(ax.pointerFrac(10, { left: 0, width: 0 }), 0);
});

for (const [name, raw] of Object.entries(golden).filter(([k]) => !k.startsWith("_"))) {
  const c = raw as GoldenCase;
  test(`T-045 ${name}: the pipeline's peak bin renders and reads back at ${c.signal_hz} Hz within one bin`, () => {
    const g = ax.geometryOf(c)!;
    const df = ax.binWidthHz(g);
    near(ax.binHz(g, c.peak_bin), c.signal_hz, df, "bin → Hz");
    for (const W of [412, 1236, 5076]) {
      for (const view of [ax.fullView(g), ax.zoomTo(g, c.signal_hz - 100e3, c.signal_hz + 100e3)]) {
        const rect = { left: 22, width: W };
        const px = ax.hzToFrac(view, ax.binHz(g, c.peak_bin)) * W;
        near(ax.fracToHz(view, ax.pointerFrac(rect.left + px, rect)), c.signal_hz, df, `readout at ${W} px`);
        // The texture column the shaders draw under that pixel is the peak bin itself.
        const [u0, u1] = ax.textureWindow(g, view);
        assert.equal(Math.floor((u0 + (px / W) * (u1 - u0)) * g.bins), c.peak_bin, `drawn column at ${W} px`);
      }
    }
    assert.match(ax.describe(g, ax.fullView(g)), /^centre 100\.8000 MHz · span 2\.4000 MHz/);
  });
}

test("zoom clamps to the band, keeps a minimum width, and maps to a texture window", () => {
  const full = ax.fullView(FM), df = ax.binWidthHz(FM);
  assert.deepEqual(ax.textureWindow(FM, full), [0, 1]);
  assert.deepEqual(ax.zoomTo(FM, 101.4e6, 101.2e6), { loHz: 101.2e6, hiHz: 101.4e6 });
  const edge = ax.zoomTo(FM, 99e6, 99.7e6);
  assert.equal(edge.loHz, full.loHz);
  near(edge.hiHz - edge.loHz, 0.7e6, 1e-3, "width kept at the edge");
  const narrow = ax.zoomTo(FM, 101.3e6, 101.3e6 + 10);
  near(narrow.hiHz - narrow.loHz, 8 * df, 1e-6, "minimum 8 bins");
  const [u0, u1] = ax.textureWindow(FM, { loHz: 100.8e6, hiHz: 101.4e6 });
  near(u0, (100.8e6 - full.loHz) / 2.4e6, 1e-12, "u0");
  near(u1, (101.4e6 - full.loHz) / 2.4e6, 1e-12, "u1");
  assert.match(ax.describe(FM, { loHz: 101.2e6, hiHz: 101.4e6 }), /view 101\.2000–101\.4000 MHz$/);
});

test("drag selection math: frequency extent, spectrum/waterfall rows, time span", () => {
  assert.deepEqual(ax.selectionHz({ loHz: 100e6, hiHz: 102e6 }, 0.75, 0.25), { loHz: 100.5e6, hiHz: 101.5e6, bandwidthHz: 1e6 });
  assert.deepEqual(ax.yHit(0.1, 0.35, 512), { area: "spectrum" });
  assert.deepEqual(ax.yHit(0.35, 0.35, 512), { area: "waterfall", rowsBack: 0 });
  assert.deepEqual(ax.yHit(0.675, 0.35, 512), { area: "waterfall", rowsBack: 256 });
  assert.deepEqual(ax.yHit(1, 0.35, 512), { area: "waterfall", rowsBack: 511 });
  // T-337: `timeSpanRows` places through the rows' own capture times, so these use the same clock
  // the waterfall would — 512 rows at 0.04 s, newest at t = 100 (or t = 10 for the scrolled-off
  // case). It answers in fractions of the waterfall pane, which is the pane the rows and the boxes
  // are both drawn in (T-362); nothing places in canvas fractions any more.
  const clock = (newestT: number) => {
    const timeAt = (n: number) => (n >= 0 && n < 512 ? newestT - n * 0.04 : NaN);
    return (t: number) => ax.rowsBackAt(timeAt, 512, t);
  };
  const span = ax.timeSpanRows(90, 100, clock(100), 512)!;
  // t = 100 is the newest row's *start*, i.e. the boundary one row down (a row's time is its first
  // sample, T-337), and t = 90 is 250 rows older than that.
  near(span[0], 1 / 512, 1e-12, "newest edge just below the top of the waterfall");
  near(span[1], 251 / 512, 1e-12, "older edge 250 rows further down");
  assert.deepEqual(ax.timeSpanRows(0, 10, clock(10 + 600 * 0.04), 512), null, "scrolled off");
  assert.equal(ax.timeSpanRows(0, 95, clock(100), 512)![1], 1, "older edge below the screen clamps to the bottom");
  assert.equal(ax.timeSpanRows(0, 50, clock(100), 512), null, "newer edge 1250 rows back: off screen");
});

test("T-337: rowsBackAt inverts the rows' own capture times, and is never a rows-per-second", () => {
  // Uneven rows (a dropped run between rows 2 and 3): the ring did not advance while time did.
  const times = [500, 499.9, 499.8, 497.0, 496.9, 496.8];
  const timeAt = (n: number) => (n >= 0 && n < times.length ? times[Math.floor(n)] : NaN);
  const back = (t: number) => ax.rowsBackAt(timeAt, times.length, t);
  // A row's time is its first sample, so row k covers [times[k], times[k-1]) and times[k] is the
  // boundary at position k+1 — row k's bottom. An emission filling row k places at exactly [k, k+1].
  for (let k = 0; k < times.length; k++) near(back(timeAt(k)), k + 1, 1e-9, `row ${k}'s start is its bottom edge`);
  near(back(500.1), 0, 1e-9, "the live edge, one row's duration past the newest row's start");
  near(back(498.4), 3.5, 1e-9, "half way through the gap row is half way down it");
  assert.ok(back(500.15) < 0 && back(496.0) > times.length, "extrapolates off both ends");
  assert.ok(Number.isNaN(ax.rowsBackAt(timeAt, 0, 500)), "no rows, no mapping");
  assert.ok(Number.isNaN(ax.rowsBackAt(timeAt, 1, 500)), "one row gives no duration to interpolate in");
});

test("ticks are round multiples inside the view; labels resolve their step", () => {
  const ts = ax.ticks({ loHz: 99.6e6, hiHz: 102e6 }, 11);
  assert.deepEqual(ts.map((t) => t.hz), [100e6, 100.5e6, 101e6, 101.5e6, 102e6]);
  assert.ok(ts.every((t) => t.frac >= 0 && t.frac <= 1));
  assert.deepEqual(ax.ticks({ loHz: 101.2e6, hiHz: 101.4e6 }, 4).map((t) => t.hz), [101.2e6, 101.25e6, 101.3e6, 101.35e6, 101.4e6]);
  assert.equal(ax.fmtMHz(100.8e6, 200e3), "100.8");
  assert.equal(ax.fmtMHz(101.2999e6, 585.9375), "101.2999");
  assert.equal(ax.fmtMHz(101.3e6, 2343.75), "101.300");
  assert.equal(ax.fmtBandwidth(1.2e6), "1.200 MHz");
  assert.equal(ax.fmtBandwidth(12.5e3), "12.50 kHz");
  assert.equal(ax.fmtBandwidth(850), "850 Hz");
});
