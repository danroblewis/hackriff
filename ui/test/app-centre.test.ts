// T-152 (ADR-0013 §4.3, §8): MUI centre pure logic — bracket placement (Hz→%, clamping, narrow rule),
// selection boxes, DC mask (GAP 10), hover readout, drag-to-select math, click target, axis ticks,
// history grid → waterfall rows, the Go to decision, and the row-preparation cost of the waterfall.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import * as ax from "../src/axis";
import { ControlError } from "../src/controls/client";
import type { Row } from "../src/inventory";
import { tickModel } from "../src/app/centre/axis-view";
import { mounts } from "../src/app/centre";
import {
  DC_NOTCH_HALF_HZ, LABEL_MIN_PX, assumedDc, bracketLayout, clickTarget, dcFromObservations, dcQuery, dragSelection, draftBox,
  hoverText, isDrag, levelU, placeExtent, regionName, selectionBoxes, selectionLabel, timeScaleText, tipOnLeft, type RowClock,
} from "../src/app/centre/overlays";
import { historyMaxCells, historyQuery, historyRows, historyWindow, parseHistory, sameCursor, type HistoryGrid } from "../src/app/centre/review-render";
import { centreInitial } from "../src/app/centre/slice";
import { geometryOfLive, gotoDecision, mayRetune, nextView, NOT_LIVE_TEXT, retuneErrorText } from "../src/app/centre/view";
import { UNOBSERVED_DB, decimateRow } from "../src/waterfall";

const G: ax.Geometry = { centerHz: 100_000_000, bandwidthHz: 2_400_000, bins: 1024 };
const V: ax.View = { loHz: 99_000_000, hiHz: 101_000_000 }; // 2 MHz view

function row(id: string, lo: number, hi: number, state: Row["state"] = "confirmed", last = 1): Row {
  return {
    id, state, f_center_hz: (lo + hi) / 2, bandwidth_hz: hi - lo, f_lo_hz: lo, f_hi_hz: hi, first_seen_s: 0, last_seen_s: last, count: 1,
    known_status: "unknown", status: null, tags: [], family: null, identity_scheme: null, identity_class: null, withheld: false, recurrence: null,
  };
}
const near = (a: number, b: number, eps = 1e-9) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

test("placeExtent maps Hz to percent of the view, clamps to it, and widens to the minimum", () => {
  const p = placeExtent(V, 99_500_000, 100_000_000)!;
  near(p.leftPct, 25); near(p.widthPct, 25);
  const c = placeExtent(V, 98_000_000, 99_200_000)!; // partly left of the view
  near(c.leftPct, 0); near(c.widthPct, 10);
  assert.equal(placeExtent(V, 101_000_001, 101_100_000), null);
  assert.equal(placeExtent(V, 98_000_000, 98_999_999), null);
  const m = placeExtent(V, 101_000_000, 101_000_000, 0.01)!; // a point on the right edge keeps its width inside
  near(m.widthPct, 1); near(m.leftPct, 99);
});

test("brackets: confirmed and candidate only, focused last, labels hidden when narrow", () => {
  const rows = [
    row("a", 99_900_000, 100_100_000), // 200 kHz = 10 % of the view
    row("b", 100_500_000, 100_510_000, "candidate"), // 10 kHz = 0.5 %
    row("d", 99_200_000, 99_300_000, "deleted"),
    row("x", 105_000_000, 105_200_000), // out of view
  ];
  const wide = bracketLayout(rows, V, 1440, "a");
  assert.deepEqual(wide.map((b) => b.id), ["b", "a"]);
  const [b, a] = wide;
  assert.equal(a.active, true); assert.equal(a.state, "confirmed"); assert.equal(a.narrow, false);
  near(a.leftPct, 45); near(a.widthPct, 10);
  assert.equal(a.label, "100.000");
  assert.equal(b.state, "candidate"); assert.equal(b.narrow, true); // 0.5 % of 1440 px = 7 px
  const phone = bracketLayout(rows, V, 400, null);
  assert.equal(phone.find((x) => x.id === "a")!.narrow, 0.1 * 400 < LABEL_MIN_PX);
  assert.ok(phone.every((x) => !x.active));
});

test("selection boxes: in view, focused active, pending flagged", () => {
  const list = [{ id: "s1", f_lo: 99_000_000, f_hi: 99_500_000 }, { id: "s2", f_lo: 102e6, f_hi: 103e6 }, { id: "s3", f_lo: 100e6, f_hi: 100.2e6 }];
  const boxes = selectionBoxes(list, V, "s3", new Set(["s1"]));
  assert.deepEqual(boxes.map((b) => [b.id, b.active, b.pending]), [["s1", false, true], ["s3", true, false]]);
  near(boxes[0].leftPct, 0); near(boxes[0].widthPct, 25);
});

test("DC mask: observation-log notch for this tune, else the documented ±15 kHz assumption", () => {
  const a = assumedDc(G);
  assert.deepEqual(a, { loHz: G.centerHz - DC_NOTCH_HALF_HZ, hiHz: G.centerHz + DC_NOTCH_HALF_HZ, assumed: true });
  const body = { records: [
    { record: "sweep" },
    { record: "dwell", window: { center_hz: 90e6, dc_excluded: { lo_hz: 89_985_000, hi_hz: 90_015_000 } } }, // another tune
    { record: "dwell", window: { center_hz: G.centerHz, dc_excluded: { lo_hz: 99_990_000, hi_hz: 100_010_000 } } },
  ] };
  assert.deepEqual(dcFromObservations(body, G), { loHz: 99_990_000, hiHz: 100_010_000, assumed: false });
  assert.equal(dcFromObservations({ records: [{ record: "sweep" }] }, G), null);
  assert.equal(dcFromObservations(null, G), null);
  assert.equal(dcFromObservations({ error: "unavailable" }, G), null);
  const q = dcQuery(G, 1000);
  assert.match(q, /^\/api\/observations\?f_lo=\d+&f_hi=\d+&t0=880&t1=1001&tier=interactive&limit=1$/);
});

test("hover readout: bin centre, level, not observed, row time", () => {
  const df = ax.binWidthHz(G); // 2343.75 Hz
  assert.equal(hoverText(G, 100_000_700, -84.26), "100.000 MHz · -84.3 dBFS/Hz"); // 2.3 kHz bins resolve 1 kHz
  assert.equal(hoverText(G, 100_001_500, NaN), `${ax.fmtMHz(100_000_000 + df, df)} MHz · – dBFS/Hz`);
  assert.equal(hoverText(G, 100e6, UNOBSERVED_DB), "100.000 MHz · not observed");
  assert.equal(hoverText(G, 100e6, -90, Date.UTC(2026, 8, 15, 12, 34, 56, 789) / 1000), "100.000 MHz · -90.0 dBFS/Hz · 12:34:56.789Z");
  near(levelU(G, G.centerHz), 512.5 / 1024, 1e-12); // DC bin centre in texture coordinates
  assert.equal(tipOnLeft(0.7), true);
  assert.equal(tipOnLeft(0.3), false);
});

test("drag: 6 px threshold; frequency-only selection, timed within the waterfall, clamped at 0 Hz", () => {
  assert.equal(isDrag(3, 4), false);
  assert.equal(isDrag(6, 0), true);
  const times = (n: number) => (n < 100 ? 1000 - n * 0.04 : NaN); // 100 rows received so far
  const clock: RowClock = { specFrac: 0.35, rows: 512, timeAt: times, rowPeriodS: 0.04 };
  const f = dragSelection(V, { x: 0.5, y: 0.1 }, { x: 0.25, y: 0.1 }, 400, clock)!;
  assert.deepEqual([f.f_lo, f.f_hi, f.t_lo], [99_500_000, 100_000_000, undefined]);
  assert.equal(f.name, regionName(99_500_000, 100_000_000));
  assert.equal(f.name, "99.50–100.00 MHz"); // resolved to 1/20 of 500 kHz
  // Both ends in the waterfall, 0.3 × 400 px of vertical travel; far end below the received rows → oldest.
  const t = dragSelection(V, { x: 0.25, y: 0.4 }, { x: 0.5, y: 0.7 }, 400, clock)!;
  const nearRow = ax.yHit(0.4, 0.35, 512) as { rowsBack: number };
  assert.equal(t.t_hi, times(nearRow.rowsBack) + 0.04);
  assert.equal(t.t_lo, times(99));
  assert.equal(dragSelection(V, { x: 0.3, y: 0.5 }, { x: 0.3, y: 0.9 }, 400, clock), null); // no width
  const low: ax.View = { loHz: -1_000_000, hiHz: 1_000_000 };
  const c = dragSelection(low, { x: 0.1, y: 0.1 }, { x: 0.75, y: 0.1 }, 400, null)!;
  assert.deepEqual([c.f_lo, c.f_hi], [0, 500_000]);
  assert.equal(dragSelection(low, { x: 0.1, y: 0.1 }, { x: 0.4, y: 0.1 }, 400, null), null); // wholly below 0 Hz
  assert.deepEqual(draftBox({ x: 0.6, y: 0.8 }, { x: 0.2, y: 0.5 }, true), { leftPct: 20, widthPct: 40, topPct: 50, heightPct: 30.000000000000004 });
  assert.deepEqual(draftBox({ x: 0.6, y: 0.8 }, { x: 0.2, y: 0.5 }, false).heightPct, 100);
  assert.equal(selectionLabel({ f_lo: 99.5e6, f_hi: 100e6, t_lo: 1, t_hi: 3.5 }, 2343.75), "99.500–100.000 MHz · 500.00 kHz · 2.50 s");
});

test("click: nearest loaded row within the half-width, else the narrowest containing selection", () => {
  const rows = [row("a", 100_000_000, 100_100_000), row("b", 100_300_000, 100_310_000, "candidate"), row("gone", 100_150_000, 100_160_000, "deleted")];
  const sels = [{ id: "wide", f_lo: 99e6, f_hi: 101e6 }, { id: "narrow", f_lo: 100.12e6, f_hi: 100.2e6 }];
  assert.deepEqual(clickTarget(rows, sels, 100_104_000, 5e3), { kind: "signal", id: "a" });
  assert.deepEqual(clickTarget(rows, sels, 100_155_000, 5e3), { kind: "selection", id: "narrow" }); // deleted row ignored
  assert.deepEqual(clickTarget(rows, sels, 99_500_000, 5e3), { kind: "selection", id: "wide" });
  assert.equal(clickTarget(rows, sels, 105e6, 5e3), null);
});

test("axis ticks: spacing by width, labels to the step", () => {
  const wide = tickModel(V, 1440), phone = tickModel(V, 400);
  assert.ok(wide.length >= 8 && wide.length <= 17, `${wide.length}`);
  assert.ok(phone.length >= 2 && phone.length <= 5, `${phone.length}`);
  assert.ok([...wide, ...phone].every((t) => t.leftPct >= 0 && t.leftPct <= 100));
  assert.deepEqual(phone.map((t) => t.label), ["99.0", "99.5", "100.0", "100.5", "101.0"]);
  assert.deepEqual(tickModel(null, 400), []);
  assert.equal(timeScaleText(512, 0.04), "↓ 20 s");
  assert.equal(timeScaleText(512, 1), "↓ 9 min");
  assert.equal(timeScaleText(512, 0), "");
});

test("history grid → waterfall rows: max over cells, null and outside grey, newest rows kept", () => {
  // 4 cells of 1 kHz from 0 Hz, 3 time rows.
  const U = Math.fround(UNOBSERVED_DB); // as stored in a Float32Array
  const grid: HistoryGrid = { f_lo_hz: 0, f_cell_hz: 1000, nf: 4, t0_s: 100, t_cell_s: 0.5, nt: 3,
    max_db: [-10, -20, null, -40, /**/ -11, -21, -31, -41, /**/ null, null, null, null] };
  assert.equal(parseHistory(grid), grid);
  assert.equal(parseHistory({ ...grid, max_db: [1, 2] }), null);
  assert.equal(parseHistory({ ...grid, f_cell_hz: 0 }), null);
  const two = historyRows(grid, { loHz: 0, hiHz: 4000 }, 2, 10);
  assert.deepEqual(two.times, [100, 100.5, 101]);
  assert.deepEqual([...two.rows[0]], [-10, -40]);
  assert.deepEqual([...two.rows[1]], [-11, -31]);
  assert.deepEqual([...two.rows[2]], [U, U]);
  const fine = historyRows(grid, { loHz: -1000, hiHz: 5000 }, 12, 2); // half-cell texels, band wider than the grid
  assert.deepEqual(fine.times, [100.5, 101]);
  const r = [...fine.rows[0]];
  assert.deepEqual(r.slice(0, 2), [U, U]);
  assert.deepEqual(r.slice(2, 10), [-11, -11, -21, -21, -31, -31, -41, -41]);
  assert.deepEqual(r.slice(10), [U, U]);
  const w = historyWindow(1000, 512, 0.04);
  near(w.t0, 1000 - 20.48, 1e-9); assert.equal(w.t1, 1000);
  assert.equal(historyMaxCells(16384, 512), 500_000);
  assert.equal(historyMaxCells(256, 512), 256 * 512);
  assert.equal(historyQuery({ loHz: -5.5, hiHz: 2000.2 }, 10, 20, 1234), "/api/history?f_lo=0&f_hi=2001&t0=10&t1=20&max_cells=1234");
  assert.equal(sameCursor({ live: true }, { live: true }), true);
  assert.equal(sameCursor({ live: false, tS: 5 }, { live: false, tS: 5 }), true);
  assert.equal(sameCursor({ live: false, tS: 5 }, { live: true }), false);
});

test("Go to: pan inside the band keeping width, retune outside when live, else not_live", () => {
  const d = gotoDecision(G, V, 100_900_000, true);
  assert.equal(d?.kind, "pan");
  const full = ax.fullView(G), pv = (d as { view: ax.View }).view;
  near(pv.hiHz - pv.loHz, 2_000_000, 1e-6);
  near(pv.hiHz, full.hiHz, 1e-6); // clamped at the band edge
  assert.deepEqual(gotoDecision(G, V, 145e6, true), { kind: "retune", centerHz: 145e6 });
  assert.deepEqual(gotoDecision(G, V, 145e6, false), { kind: "not_live" });
  assert.deepEqual(gotoDecision(null, null, 145e6, true), { kind: "retune", centerHz: 145e6 });
  assert.equal(gotoDecision(G, V, NaN, true), null);
  assert.equal(mayRetune({ loaded: false, live: false }), true);
  assert.equal(mayRetune({ loaded: true, live: false }), false);
  assert.equal(retuneErrorText(new ControlError(409, "not_live", "replay")), NOT_LIVE_TEXT);
  assert.match(retuneErrorText(new ControlError(400, "invalid", "bad centre")), /refused: bad centre/);
});

test("view after a header: kept, clamped, re-centred after a retune, or the requested one", () => {
  const full = ax.fullView(G);
  assert.deepEqual(nextView(G, null), full);
  const kept = nextView(G, V);
  near(kept.loHz, V.loHz, 1e-6); near(kept.hiHz, V.hiHz, 1e-6);
  const moved = nextView(G, { loHz: 144e6, hiHz: 144.5e6 }); // old tune's view, wholly outside
  near(moved.hiHz - moved.loHz, 500_000, 1e-6); near((moved.loHz + moved.hiHz) / 2, G.centerHz, 1e-6);
  const want = nextView(G, V, { loHz: 100.2e6, hiHz: 100.4e6 });
  near(want.loHz, 100.2e6, 1e-6); near(want.hiHz, 100.4e6, 1e-6);
  assert.deepEqual(geometryOfLive(centreInitial().live), null);
  assert.deepEqual(geometryOfLive({ centerHz: 1, bandwidthHz: 2, bins: 3 }), { centerHz: 1, bandwidthHz: 2, bins: 3 });
  assert.equal(centreInitial().live.pendingView, null);
});

test("waterfall row preparation: max-pool decimation is exact and cheap at 16k texels", () => {
  const db = new Float32Array([1, 5, 2, 2, -1, -3, 7, 0]);
  assert.deepEqual([...decimateRow(db, 4)], [5, 2, -1, 7]);
  assert.deepEqual([...decimateRow(db, 8)], [...db]);
  assert.notEqual(decimateRow(db, 8), db); // a copy: the stream's buffer is reused
  assert.deepEqual([...decimateRow(new Float32Array([3, 9]), 4)], [3, 3, 9, 9]);
  // Budget: the per-row CPU work before a texSubImage2D upload. 16384 bins at 25 rows/s (as-is) and
  // 65536 → 16384 must each stay far below a 16.7 ms frame; asserted loosely (≤ 2 ms mean) for CI.
  const big = new Float32Array(65536).map((_, i) => Math.sin(i)), same = new Float32Array(16384).map((_, i) => Math.cos(i));
  const bench = (f: () => void, n: number) => { const t = performance.now(); for (let i = 0; i < n; i++) f(); return (performance.now() - t) / n; };
  bench(() => decimateRow(big, 16384), 5);
  const copyMs = bench(() => decimateRow(same, 16384), 200), poolMs = bench(() => decimateRow(big, 16384), 50);
  console.log(`# decimateRow: 16384→16384 ${copyMs.toFixed(3)} ms/row, 65536→16384 ${poolMs.toFixed(3)} ms/row`);
  assert.ok(copyMs < 2 && poolMs < 2, `row prep too slow: ${copyMs} / ${poolMs} ms`);
});

test("centre mounts both centre slots; CSS is scoped to them with no wide min-width", () => {
  assert.deepEqual(Object.keys(mounts).sort(), ["axis", "live"]);
  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  const selectors = [...css.matchAll(/([^{}@]+)\{[^{}]*\}/g)].map((m) => m[1].trim()).filter((s) => s && !s.startsWith("@"));
  assert.ok(selectors.length > 10);
  for (const sel of selectors) for (const part of sel.split(",")) assert.match(part.trim(), /^\.(specwf|axis)\b/, `unscoped: ${part}`);
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400);
  assert.match(readFileSync("src/app/base.css", "utf8"), /\.live-canvas \{[^}]*touch-action: none/);
});
