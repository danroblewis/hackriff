// T-152 (ADR-0013 §4.3, §8): MUI centre pure logic — bracket placement (Hz→%, clamping, narrow rule),
// selection boxes, DC mask (GAP 10), hover readout, drag-to-select math, click target, axis ticks,
// history grid → waterfall rows, the Go to decision, and the row-preparation cost of the waterfall.
// T-193 (docs/14-ui-rewrite.md "Added scope from docs/15 §7"): the Confirmed-signal band box (one
// per row, spanning both panes), its draggable-edge hit-testing and drag→band mapping.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import * as ax from "../src/axis";
import { ControlError, reactionTo } from "../src/controls/client";
import type { Presence, Row, UserBand } from "../src/inventory";
import { retuneOfferModel, tickModel } from "../src/app/centre/axis-view";
import { mounts } from "../src/app/centre";
import {
  DC_NOTCH_HALF_HZ, EDGE_HIT_PX, LABEL_MIN_PX, addModeActive, assumedDc, bandEdgeHit, bracketLayout, clickTarget, confirmedBands, confirmedEdgeAt,
  dcFromHeader, dcFromObservations, dcQuery, dragBandEdge, dragSelection, draftBox, effectiveBand, hoverText, isDrag, levelU, minUserBandHz,
  placeExtent, presenceBoxes, regionName, selectionBoxes, selectionTimeBoxes, selectionLabel, snapFracToPixel, timeScaleText, tipOnLeft, type RowClock,
} from "../src/app/centre/overlays";
import { placeTimeBoxes, type TimeBox } from "../src/timebox";
import { historyMaxCells, historyQuery, historyRows, historyWindow, parseHistory, sameCursor, type HistoryGrid } from "../src/app/centre/review-render";
import { centreInitial } from "../src/app/centre/slice";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { applyDeviceAction, centreView, centreViewKey, geometryOfLive, gotoDecision, mayRetune, nextView, NOT_LIVE_TEXT, retuneAction, retuneErrorText, viewHooks } from "../src/app/centre/view";
import { UNOBSERVED_DB, decimateRow } from "../src/waterfall";

const G: ax.Geometry = { centerHz: 100_000_000, bandwidthHz: 2_400_000, bins: 1024 };
const V: ax.View = { loHz: 99_000_000, hiHz: 101_000_000 }; // 2 MHz view

function userBand(fLo: number, fHi: number): UserBand {
  return { f_lo: fLo, f_hi: fHi, set_at: 1, actor: "fp", reason: null, reason_withheld: false };
}

function row(id: string, lo: number, hi: number, state: Row["state"] = "confirmed", last = 1, ub: UserBand | null = null, presence?: Presence, family: string | null = null): Row {
  return {
    id, state, f_center_hz: (lo + hi) / 2, bandwidth_hz: hi - lo, f_lo_hz: lo, f_hi_hz: hi, first_seen_s: 0, last_seen_s: last, count: 1,
    known_status: "unknown", status: null, tags: [], family, identity_scheme: null, identity_class: null, withheld: false, recurrence: null,
    user_band: ub, presence,
  };
}

/** A `presence.last_interval` (docs/api.md `presence`, T-284): `open` defaults to the interval
 * still being live at the request window's own edge. */
function iv(t0: number, t1: number, open = true): Presence {
  return { intervals: 1, on_air_s: t1 - t0, last_interval: { t_start_s: t0, t_end_s: t1, open }, liveness: open ? "live" : "ended", ended_t_s: open ? null : t1 };
}
const near = (a: number, b: number, eps = 1e-9) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

/**
 * A [[RowClock]] over an explicit list of row capture times, newest first — the same construction
 * `Waterfall` makes over the timestamps the backend served with each row (`Waterfall.timeAt` /
 * `Waterfall.rowsBackAt`, both over `axis.rowsBackAt`). `declaredHz` is the *declared* row rate the
 * spectrum header carries, which is free to disagree with the times: that disagreement is the
 * whole point of T-337.
 */
function rowClock(times: readonly number[], declaredHz = 25, rows = 512, specFrac = 0.35): RowClock {
  const timeAt = (n: number) => (n >= 0 && n < rows && n < times.length ? times[Math.floor(n)] : NaN);
  return { specFrac, rows, timeAt, rowsBackAt: (t) => ax.rowsBackAt(timeAt, Math.min(times.length, rows), t), rowPeriodS: 1 / declaredHz };
}

/** Row times, newest first, at a steady `periodS`. */
const steady = (newestT: number, n: number, periodS: number) => Array.from({ length: n }, (_, k) => newestT - k * periodS);

/**
 * Where the waterfall's render pass puts a [[TimeBox]] (T-362): exactly `placeTimeBoxes` over the
 * clock's own `rowsBackAt` and the zoom window the rows are drawn with, reported in the canvas
 * percentages the DOM overlay used to use so the T-337 assertions below still read on the axis they
 * were written for. `rowTopPct`/`rowHeightPct` are the same rectangle as fractions of the waterfall
 * pane. Null when the box has scrolled off the rows held — the render pass draws nothing there.
 */
function drawn(b: TimeBox | undefined, c: RowClock, uw: readonly [number, number] = ax.textureWindow(G, V)) {
  if (!b) return null;
  const [p] = placeTimeBoxes([b], c.rowsBackAt, c.rows, uw[0], uw[1]);
  if (!p) return null;
  return {
    leftPct: p.x0 * 100, widthPct: (p.x1 - p.x0) * 100,
    rowTopPct: p.y0 * 100, rowHeightPct: (p.y1 - p.y0) * 100,
    topPct: (c.specFrac + p.y0 * (1 - c.specFrac)) * 100,
    heightPct: (p.y1 - p.y0) * (1 - c.specFrac) * 100,
  };
}

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

test("brackets: Candidate AND Confirmed rows (T-389), never deleted ones; the focused one last; labels hidden when narrow", () => {
  const rows = [
    row("a", 99_900_000, 100_100_000), // confirmed: 200 kHz = 10 % of the view
    row("b", 100_500_000, 100_510_000, "candidate"), // 10 kHz = 0.5 %
    row("d", 99_200_000, 99_300_000, "deleted"),
    row("x", 105_000_000, 105_200_000, "candidate"), // out of view
  ];
  const wide = bracketLayout(rows, V, 1440, "b");
  // T-389: confirmed is here again. T-193 moved it to the full-height band box and T-261 then
  // narrowed that box to the focused row, which left an UNSELECTED confirmed row with no marker in
  // the spectrum pane at all — the user's "the confirmed box appears only after I click it".
  assert.deepEqual(wide.map((b) => b.id), ["a", "b"], "the focused row sorts last so it draws on top");
  assert.equal(wide.find((x) => x.id === "a")!.state, "confirmed");
  const b = wide.find((x) => x.id === "b")!;
  assert.equal(b.active, true); assert.equal(b.state, "candidate"); assert.equal(b.narrow, true); // 0.5 % of 1440 px = 7 px
  const phone = bracketLayout(rows, V, 400, null);
  assert.equal(phone.find((x) => x.id === "b")!.narrow, true);
  assert.ok(phone.every((x) => !x.active));
});

// ---- T-193: Confirmed-signal band box, its edges, and the drag → band mapping ----

test("effectiveBand: the measured extent, or the user override's edges when set", () => {
  const r = row("a", 100_000_000, 100_100_000);
  assert.deepEqual(effectiveBand(r), { loHz: 100_000_000, hiHz: 100_100_000, hasUserBand: false });
  const withUb = row("a", 100_000_000, 100_100_000, "confirmed", 1, userBand(99_990_000, 100_120_000));
  assert.deepEqual(effectiveBand(withUb), { loHz: 99_990_000, hiHz: 100_120_000, hasUserBand: true });
});

test("confirmedBands: one box per Confirmed row, candidates/deleted excluded, focused last, measured-edge ticks when a user band moved off them", () => {
  const rows = [
    row("a", 99_900_000, 100_100_000), // confirmed, no override
    row("b", 99_950_000, 100_050_000, "confirmed", 1, userBand(99_920_000, 100_010_000)), // moved both edges
    row("c", 100_400_000, 100_410_000, "candidate"), // never a band box
    row("d", 99_200_000, 99_300_000, "deleted"),
  ];
  const boxes = confirmedBands(rows, V, "a");
  assert.deepEqual(boxes.map((x) => x.id), ["b", "a"]); // "a" focused, drawn last
  const [b, a] = boxes;
  assert.equal(a.active, true); assert.equal(a.hasUserBand, false);
  assert.equal(a.measuredLeftPct, null); assert.equal(a.measuredRightPct, null);
  near(a.leftPct, 45); near(a.widthPct, 10);
  assert.equal(b.active, false); assert.equal(b.hasUserBand, true);
  // The box is drawn at the user band's edges; the measured edges (99.95/100.05 MHz) get a tick.
  near(b.leftPct, ax.hzToFrac(V, 99_920_000) * 100); near((b.leftPct + b.widthPct), ax.hzToFrac(V, 100_010_000) * 100);
  near(b.measuredLeftPct!, ax.hzToFrac(V, 99_950_000) * 100);
  near(b.measuredRightPct!, ax.hzToFrac(V, 100_050_000) * 100);
});

test("confirmedBands: an override (an in-progress drag) replaces the row's band and always counts as a user band", () => {
  const rows = [row("a", 99_900_000, 100_100_000)];
  const boxes = confirmedBands(rows, V, null, new Map([["a", { loHz: 99_950_000, hiHz: 100_050_000 }]]));
  assert.equal(boxes.length, 1);
  assert.equal(boxes[0].hasUserBand, true);
  near(boxes[0].leftPct, 47.5); near(boxes[0].widthPct, 5);
});

test("presenceBoxes (T-261, ADR-0017 TM-4): centre/width from f_lo/f_hi, time extent from presence.last_interval, growth is a plain redraw", () => {
  const clock = rowClock(steady(1000, 512, 0.04), 25); // newest row (rowsBack 0) at t=1000
  // 5 s ago .. now (still open): near the live edge (top of the waterfall), a few rows tall.
  const a = row("a", 99_900_000, 100_100_000, "confirmed", 1, null, iv(995, 1000, true));
  const boxes = presenceBoxes([a], G, null);
  assert.equal(boxes.length, 1);
  const [t] = boxes;
  assert.equal(t.id, "a"); assert.equal(t.state, "confirmed"); assert.equal(t.open, true); assert.equal(t.chirp, false);
  // The box itself is a region, not a rectangle: a band fraction and the interval's own two times,
  // straight off the API, with no screen coordinate to go stale between polls (T-362).
  assert.deepEqual([t.tLo, t.tHi], [995, 1000]);
  near(t.u0, ax.hzToFrac(ax.fullView(G), 99_900_000)); near(t.u1, ax.hzToFrac(ax.fullView(G), 100_100_000));
  const b = drawn(t, clock)!;
  near(b.leftPct, 45); near(b.widthPct, 10); // same frequency placement as placeExtent/confirmedBands
  const expected = ax.timeSpanRows(995, 1000, clock.rowsBackAt, 512)!;
  near(b.topPct, (0.35 + expected[0] * 0.65) * 100); near(b.heightPct, (expected[1] - expected[0]) * 0.65 * 100);
  // `t_end_s` is the newest row's own start time, which is the boundary below it: the emission
  // covered row 1 and not row 0, so the box starts one row down, not at the top of the pane.
  near(b.topPct, (0.35 + (1 / 512) * 0.65) * 100, 1e-9);
  // The next poll's t_end_s advanced (still open, more evidence arrived): the SAME box, redrawn,
  // is taller — nothing here is animated, it is only a fresh call with the API's new numbers.
  const grown = drawn(presenceBoxes([row("a", 99_900_000, 100_100_000, "confirmed", 1, null, iv(995, 1004, true))], G, null)[0], rowClock(steady(1004, 512, 0.04), 25))!;
  assert.ok(grown.heightPct > b.heightPct, "the open interval's box grew");
});

test("presenceBoxes: never fabricates a box — no interval, no presence at all, or the focused row (kept on the full-height bracket instead)", () => {
  const clock = rowClock(steady(1000, 512, 0.04), 25);
  const noPresence = row("a", 99_900_000, 100_100_000); // pre-T-284 fixture: presence undefined
  const noInterval: Row = { ...row("b", 99_900_000, 100_100_000), presence: { intervals: 0, on_air_s: 0, last_interval: null, liveness: "absent", ended_t_s: null } };
  const focused = row("c", 99_900_000, 100_100_000, "confirmed", 1, null, iv(995, 1000));
  assert.deepEqual(presenceBoxes([noPresence, noInterval], G, null), []);
  assert.deepEqual(presenceBoxes([focused], G, "c"), [], "the focused row keeps its existing full-height box, not this one");
  // Scrolled entirely off the waterfall's own history: the row still has an interval, so it is
  // still a box — and the render pass draws nothing for it, rather than a rectangle clamped to a
  // duration it never had. Since T-362 that judgement belongs where the rows are, not to the poll.
  const stale = row("d", 99_900_000, 100_100_000, "confirmed", 1, null, iv(0, 1));
  assert.equal(drawn(presenceBoxes([stale], G, null)[0], clock), null);
});

test("presenceBoxes: a row classified as css/chirp is flagged so the box is labelled a bounding box, not a swept polyline (ADR-0017 §1.3)", () => {
  const clock = rowClock(steady(1000, 512, 0.04), 25);
  const chirp = row("a", 99_900_000, 100_100_000, "confirmed", 1, null, iv(995, 1000), "css");
  const fm = row("b", 100_400_000, 100_410_000, "confirmed", 1, null, iv(995, 1000), "wfm-broadcast");
  const boxes = presenceBoxes([chirp, fm], G, null);
  assert.equal(boxes.find((x) => x.id === "a")!.chirp, true);
  assert.equal(boxes.find((x) => x.id === "b")!.chirp, false);
  assert.equal(boxes.find((x) => x.id === "a")!.style.hatch, true, "and the hatch says so on screen");
  assert.ok(boxes.find((x) => x.id === "a")!.title!.includes("bounding box"), "and the hover readout says why");
  assert.ok(drawn(boxes[0], clock), "still placed like any other box");
});

// ---- T-337: one shared time axis ----------------------------------------------------------
//
// The user's invariant (CLAUDE.md, "Time, the waterfall, and the live view"): for the current view
// there is ONE canonical mapping between absolute capture time and screen position, and everything
// time-varying is laid out through it and moves together. These test the invariant, not the
// rendering: a box's placement must be a *pure function of its capture time* under the very mapping
// the waterfall rows use, so a box and a row that share a capture time land in the same place.
//
// The rows-per-second the backend *declares* is not that mapping, and the clocks below say why:
//   - a gated spectrum stream declares a rate deliberately up to 10 % above the actual row rate
//     (`hk_pipeline::class::RowPlan::declared_hz`);
//   - a gated row, a dropped run or a backlog-skipped frame advances capture time without
//     advancing the ring, so rows are not evenly spaced in time at all.
// A presence test ("a box was drawn") passes under either mapping. These fail under the wrong one.

test("T-337 invariant: the row clock is the one mapping — a row's start time lands on that row's own bottom boundary, however uneven the rows are", () => {
  // 40 ms rows with a 1.2 s gap after row 9 (a dropped/gated run: capture time advanced, the ring
  // did not) and a slower stretch after that. Nothing here is a multiple of any single period.
  const times = [1000, 999.96, 999.92, 999.88, 999.84, 999.8, 999.76, 999.72, 999.68, 999.64, 998.44, 998.33, 998.2, 998.02, 997.5];
  const c = rowClock(times, 25);
  // A row's time is the FIRST sample of its span, so row k covers [times[k], times[k-1]) and
  // times[k] is the boundary at position k+1 — the bottom of row k, the top of row k+1.
  for (let k = 0; k < times.length; k++) near(c.rowsBackAt(c.timeAt(k)), k + 1, 1e-9);
  // Inside row 10's own duration (times[10]..times[9]): the fraction of it still to come.
  near(c.rowsBackAt(0.5 * (times[10] + times[9])), 10.5);
  // Off either end it extrapolates from the nearest boundary pair rather than lying about a row.
  near(c.rowsBackAt(1000.04), 0, 1e-9); // the live edge: one row's duration past the newest row's start
  assert.ok(c.rowsBackAt(1000.06) < 0, "past the live edge is negative rows-back");
  assert.ok(c.rowsBackAt(997.0) > times.length - 1, "older than the oldest row held is past the last row");
  assert.ok(Number.isNaN(c.rowsBackAt(NaN)) && Number.isNaN(rowClock([], 25).rowsBackAt(1000)), "no rows, no answer");
  assert.ok(Number.isNaN(rowClock([1000], 25).rowsBackAt(1000)), "one row gives no duration to place anything in");
});

test("T-337 invariant: a box spanning exactly row k lands exactly on row k — the pixels yHit gives that row", () => {
  const times = [1000, 999.96, 999.92, 998.72, 998.68, 998.64, 998.6, 997.4]; // two dropped runs
  const c = rowClock(times, 25);
  for (let k = 1; k < times.length; k++) {
    // A signal present for exactly row k: its own capture time up to the next-newer row's.
    const r = row(`r${k}`, 99_900_000, 100_100_000, "confirmed", 1, null, iv(times[k], times[k - 1], false));
    const b = drawn(presenceBoxes([r], G, null)[0], c)!;
    // The canvas fractions row k occupies, straight off the row index — the waterfall's own layout.
    const top = c.specFrac + (k / c.rows) * (1 - c.specFrac), bottom = c.specFrac + ((k + 1) / c.rows) * (1 - c.specFrac);
    near(b.topPct, top * 100, 1e-9);
    near(b.topPct + b.heightPct, bottom * 100, 1e-9);
    // And the round trip through yHit: the box's own top pixel hit-tests as row k.
    const hit = ax.yHit(top + 1e-9, c.specFrac, c.rows);
    assert.equal(hit.area === "waterfall" && hit.rowsBack, k);
  }
});

test("T-337 invariant: placement is a pure function of capture time — the declared row rate cannot move a box", () => {
  const times = steady(1000, 64, 0.04);
  const interval = iv(999.2, 1000, true);
  const r = row("a", 99_900_000, 100_100_000, "confirmed", 1, null, interval);
  // Three wildly different *declared* rates over identical rows: identical boxes.
  const boxes = [25, 27.5, 4].map((hz) => drawn(presenceBoxes([r], G, null)[0], rowClock(times, hz))!);
  for (const b of boxes.slice(1)) {
    near(b.topPct, boxes[0].topPct, 1e-12);
    near(b.heightPct, boxes[0].heightPct, 1e-12);
  }
});

test("T-337 drift: a gated stream's declared rate is 10 % fast, and placing a box by it walks the box off its energy", () => {
  // `RowPlan::declared_hz = min(row_rate_hz * 1.1, 50)` on a class that forbids content, so a UI
  // reading `sample_rate_hz` as a row clock thinks rows are 10 % closer together than they are.
  const actualHz = 25, declaredHz = actualHz * 1.1;
  const times = steady(1000, 512, 1 / actualHz);
  const c = rowClock(times, declaredHz);
  // A one-row-long emission 400 rows back (16 s of capture): the box must sit on row 400.
  const r = row("a", 99_900_000, 100_100_000, "confirmed", 1, null, iv(times[400], times[399], false));
  const b = drawn(presenceBoxes([r], G, null)[0], c)!;
  near(b.topPct, (c.specFrac + (400 / c.rows) * (1 - c.specFrac)) * 100, 1e-9);
  // What the declared rate would have said: the age of the box ÷ a 10 % short row period.
  const wrongBack = (times[0] - times[400]) * declaredHz;
  assert.ok(wrongBack - 400 > 39, `the declared rate misplaces it by ${wrongBack - 400} rows`);
  assert.ok(Math.abs(b.topPct - (c.specFrac + (wrongBack / c.rows) * (1 - c.specFrac)) * 100) > 4,
    "and that is >4 % of the canvas — visible drift, growing with age, not a rounding nit");
});

test("T-337: selections with a time extent are laid out on the same axis, and the drag→draw round trip is the identity", () => {
  const times = [1000, 999.96, 998.6, 998.56, 998.52, 997.1]; // a dropped run between rows 1 and 2
  const c = rowClock(times, 25, 16); // a 16-row waterfall, so a few rows is a real drag distance
  // Drag from row 4 to row 1 (fractions of the canvas inside the waterfall pane).
  const yOf = (k: number) => c.specFrac + ((k + 0.5) / c.rows) * (1 - c.specFrac);
  const sel = dragSelection(V, { x: 0.45, y: yOf(4) }, { x: 0.55, y: yOf(1) }, 1000, c)!;
  assert.equal(sel.t_lo, times[4], "the older edge is row 4's own capture time");
  assert.equal(sel.t_hi, times[0], "the newer edge closes row 1 at row 0's capture time, not at a nominal period past it");
  // Drawing it back through the same axis lands on exactly rows 1..4, uneven rows and all. Since
  // T-362 a timed selection is a render-pass box like a presence box, placed the same way.
  const b = drawn(selectionTimeBoxes([{ id: "s", name: "s", f_lo: sel.f_lo, f_hi: sel.f_hi, t_lo: sel.t_lo!, t_hi: sel.t_hi! }], G, null)[0], c)!;
  near(b.rowTopPct, (1 / c.rows) * 100, 1e-9);
  near(b.rowTopPct + b.rowHeightPct, (5 / c.rows) * 100, 1e-9);
  assert.deepEqual(ax.yHit(c.specFrac + (b.rowTopPct / 100 + 1e-9) * (1 - c.specFrac), c.specFrac, c.rows), { area: "waterfall", rowsBack: 1 });
  // A selection with no time extent has no time axis to sit on: it stays a full-height DOM box —
  // "any time", honestly drawn, not placed at 0 — and is never handed to the render pass.
  const [u] = selectionBoxes([{ id: "u", f_lo: 99_500_000, f_hi: 99_600_000 }], V, null);
  assert.deepEqual([u.leftPct > 0, u.widthPct > 0], [true, true]);
  assert.deepEqual(selectionTimeBoxes([{ id: "u", name: "u", f_lo: 99_500_000, f_hi: 99_600_000 }], G, null), []);
  assert.deepEqual(selectionBoxes([{ id: "s", f_lo: sel.f_lo, f_hi: sel.f_hi, t_lo: sel.t_lo, t_hi: sel.t_hi }], V, null), [],
    "and a timed one is never also a DOM box: one overlay, one placement");
  // And one whose span has scrolled off the rows held draws nothing rather than a clamped box.
  assert.equal(drawn(selectionTimeBoxes([{ id: "o", name: "o", f_lo: 99_500_000, f_hi: 99_600_000, t_lo: 10, t_hi: 11 }], G, null)[0], c), null);
});

test("T-337: the time-scale label measures the rows on screen instead of asserting rows × declared period", () => {
  // 512 rows declared at 25/s would read "↓ 20 s"; only 100 rows have arrived, over 8 s of capture.
  const c = rowClock(steady(1000, 100, 0.08), 25);
  assert.equal(timeScaleText(c.rows, c.rowPeriodS, c), "↓ 8 s");
  assert.equal(timeScaleText(512, 0.04), "↓ 20 s", "no clock yet: the declared span is the only thing there is to say");
});

test("bandEdgeHit: within EDGE_HIT_PX of the drawn left/right edge, else null", () => {
  const box = { leftPct: 25, widthPct: 25 }; // 250..500 px of a 1000 px view
  assert.equal(bandEdgeHit(250, box, 1000), "lo");
  assert.equal(bandEdgeHit(250 + EDGE_HIT_PX, box, 1000), "lo");
  assert.equal(bandEdgeHit(250 + EDGE_HIT_PX + 1, box, 1000), null);
  assert.equal(bandEdgeHit(500, box, 1000), "hi");
  assert.equal(bandEdgeHit(500 - EDGE_HIT_PX, box, 1000), "hi");
  assert.equal(bandEdgeHit(375, box, 1000), null, "the interior is not a hit: region-select/click keep priority there");
});

test("confirmedEdgeAt: resolves the topmost (focused-first) Confirmed band's edge under the pointer", () => {
  const rows = [row("a", 99_900_000, 100_100_000), row("b", 100_500_000, 100_700_000)];
  // "b" is focused, so it draws last and wins a hit that lands on both.
  const hitB = confirmedEdgeAt(rows, V, 1440, Math.round(ax.hzToFrac(V, 100_500_000) * 1440), "b");
  assert.deepEqual(hitB, { id: "b", edge: "lo" });
  const hitA = confirmedEdgeAt(rows, V, 1440, Math.round(ax.hzToFrac(V, 99_900_000) * 1440), "b");
  assert.deepEqual(hitA, { id: "a", edge: "lo" });
  assert.equal(confirmedEdgeAt(rows, V, 1440, 700, "b"), null, "mid-view, nowhere near an edge");
});

test("minUserBandHz / snapFracToPixel: a few pixels' worth of the view, and fraction rounded to a device pixel", () => {
  const perPx = (V.hiHz - V.loHz) / 1440;
  near(minUserBandHz(V, 1440), 4 * perPx);
  near(minUserBandHz(V, 1440, 10), 10 * perPx);
  assert.equal(minUserBandHz(V, 0), 1);
  assert.equal(snapFracToPixel(0.50037, 1000), 0.5);
  assert.equal(snapFracToPixel(0.5, 0), 0.5, "an unknown width leaves the fraction alone");
});

test("dragBandEdge: snaps to a pixel, clamps to the minimum width, never crosses 0 Hz", () => {
  const lo = dragBandEdge(V, 1440, "lo", 99_950_000, 100_050_000, ax.hzToFrac(V, 99_970_000));
  near(lo.hiHz, 100_050_000);
  near(lo.loHz, ax.fracToHz(V, snapFracToPixel(ax.hzToFrac(V, 99_970_000), 1440)));
  const hi = dragBandEdge(V, 1440, "hi", 99_950_000, 100_050_000, ax.hzToFrac(V, 100_070_000));
  near(hi.loHz, 99_950_000);
  near(hi.hiHz, ax.fracToHz(V, snapFracToPixel(ax.hzToFrac(V, 100_070_000), 1440)));
  // Dragging the low edge past the high edge (minus the min width) clamps instead of inverting.
  const clampedLo = dragBandEdge(V, 1440, "lo", 99_950_000, 100_050_000, ax.hzToFrac(V, 100_200_000));
  const minW = minUserBandHz(V, 1440);
  near(clampedLo.loHz, 100_050_000 - minW);
  assert.ok(clampedLo.loHz < clampedLo.hiHz);
  // A drag at/below 0 Hz never crosses it.
  const low: ax.View = { loHz: -1_000_000, hiHz: 1_000_000 };
  const atZero = dragBandEdge(low, 1440, "lo", 100_000, 500_000, 0);
  assert.equal(atZero.loHz, 0);
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

test("DC mask: the spectrum stream header's own dc_excluded_hz is preferred over the fallbacks (T-167)", () => {
  assert.deepEqual(dcFromHeader({ dc_excluded_hz: 15_000 }, G), { loHz: G.centerHz - 15_000, hiHz: G.centerHz + 15_000, assumed: false });
  assert.equal(dcFromHeader({ dc_excluded_hz: 0 }, G), null, "a non-positive width is not a mask");
  assert.equal(dcFromHeader({ dc_excluded_hz: null }, G), null, "absent on older servers or unmasked streams");
  assert.equal(dcFromHeader({}, G), null);
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
  const rowTimes = steady(1000, 100, 0.04); // 100 rows received so far
  const times = (n: number) => (n < 100 ? rowTimes[n] : NaN);
  const clock = rowClock(rowTimes, 25);
  const f = dragSelection(V, { x: 0.5, y: 0.1 }, { x: 0.25, y: 0.1 }, 400, clock)!;
  assert.deepEqual([f.f_lo, f.f_hi, f.t_lo], [99_500_000, 100_000_000, undefined]);
  assert.equal(f.name, regionName(99_500_000, 100_000_000));
  assert.equal(f.name, "99.50–100.00 MHz"); // resolved to 1/20 of 500 kHz
  // Both ends in the waterfall, 0.3 × 400 px of vertical travel; far end below the received rows → oldest.
  const t = dragSelection(V, { x: 0.25, y: 0.4 }, { x: 0.5, y: 0.7 }, 400, clock)!;
  const nearRow = ax.yHit(0.4, 0.35, 512) as { rowsBack: number };
  // T-337: the newer edge closes at the *next* row's own capture time, not a declared period past it.
  assert.equal(t.t_hi, times(nearRow.rowsBack - 1));
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

test("addModeActive (T-194): the toggle or Shift puts a drag into add mode", () => {
  assert.equal(addModeActive(false, false), false);
  assert.equal(addModeActive(true, false), true);
  assert.equal(addModeActive(false, true), true);
  assert.equal(addModeActive(true, true), true);
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
  // T-334: the view's own budgets go on the request, so the backend serves the span at a matched
  // resolution instead of leaving the client to reduce whatever shape fits the product.
  assert.equal(
    historyQuery({ loHz: 0, hiHz: 1000 }, 10, 20, 1234, 512, 2048),
    "/api/history?f_lo=0&f_hi=1000&t0=10&t1=20&max_cells=1234&max_t=512&max_f=2048",
  );
  assert.equal(historyQuery({ loHz: 0, hiHz: 1000 }, 10, 20, 1234, 0, 0), "/api/history?f_lo=0&f_hi=1000&t0=10&t1=20&max_cells=1234");
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

test("centre mounts every centre slot; CSS is scoped to them with no wide min-width", () => {
  // T-340 adds the two edge navigators, one parallel to each waterfall axis.
  assert.deepEqual(Object.keys(mounts).sort(), ["axis", "freqnav", "live", "timenav"]);
  const css = readFileSync("src/app/centre/centre.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  const selectors = [...css.matchAll(/([^{}@]+)\{[^{}]*\}/g)].map((m) => m[1].trim()).filter((s) => s && !s.startsWith("@"));
  assert.ok(selectors.length > 10);
  for (const sel of selectors) for (const part of sel.split(",")) assert.match(part.trim(), /^\.(specwf|axis|freqnav|timenav)\b/, `unscoped: ${part}`);
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400);
  assert.match(readFileSync("src/app/base.css", "utf8"), /\.live-canvas \{[^}]*touch-action: none/);
});

// ---------------------------------------------------------------------------
// T-343: a retune is a device action, and a pan is not.
//
// The defect T-339's audit found was in this exact seam: `gestures.ts` fired `hooks.overflow(...)`
// on pointerup once an accumulated pan passed 5 % of the view width, and that was wired straight to
// `POST /api/control/center` — which stops and re-plumbs the running capture when the window's
// class or rate changes. So the control below (a pan far past the old threshold reaching nothing)
// is the assertion that matters; without it the property could be satisfied by deleting the retune.
// ---------------------------------------------------------------------------

/** A context whose client records every call, so "reached the device" is observable. */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: { ...s.device, loaded: true, live, deviceId: "hackrf:0000000000000000fake0000000000ab" },
    live: { ...s.live, centerHz: 100e6, bandwidthHz: 2.4e6, bins: 1024, view: { loHz: 99.5e6, hiHz: 100.5e6 } },
  }));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

test("T-343 control: panning past the band edge reaches no device route, at any overflow", () => {
  const { ctx, calls } = deviceSpyCtx();
  const hooks = viewHooks(ctx);
  const g = geometryOfLive(ctx.store.get().live)!;

  // A pan that ends far past the edge — twenty times the old 5 % overflow threshold, the case that
  // used to POST /api/control/center on pointerup.
  const v = ctx.store.get().live.view!;
  const w = v.hiHz - v.loHz;
  for (const overflowHz of [0.06 * w, w, 10 * w, -0.06 * w, -w, -10 * w]) {
    const p = ax.panView(g, v, overflowHz);
    hooks.setView(p.view);
    hooks.edgeOffer(ax.panRetuneCenter(p.view, p.overflowHz), { loHz: p.view.loHz + p.overflowHz, hiHz: p.view.hiHz + p.overflowHz });
  }
  assert.deepEqual(calls, [], "a pan must never reach the control API");
  // The view moved and the pan left an offer behind — it is not that the gesture did nothing.
  assert.notDeepEqual(ctx.store.get().live.view, v, "the pan still panned");
  assert.ok(ctx.store.get().live.retuneOffer, "the pan offers a retune instead of performing one");
});

test("T-343: only an explicit device action reaches /api/control/center, and it names the device", async () => {
  const { ctx, calls } = deviceSpyCtx();
  await applyDeviceAction(ctx, retuneAction(99_123_456.7, "edge-offer", { loHz: 98e6, hiHz: 100e6 }));
  assert.deepEqual(calls, [{ method: "POST", path: "/api/control/center", body: { center_hz: 99_123_457 } }]);
  // Accepting the offer clears it, and the view it asked for is pending until the next header.
  assert.equal(ctx.store.get().live.retuneOffer, null);
  assert.deepEqual(ctx.store.get().live.pendingView, { loHz: 98e6, hiHz: 100e6 });
  assert.match(ctx.store.get().toast.text, /Retuning hackrf:.* to 99\.1235 MHz/);
});

test("T-343: a replay offers nothing and reaches nothing — the device is not there to move", async () => {
  const { ctx, calls } = deviceSpyCtx(false);
  viewHooks(ctx).edgeOffer(105e6, { loHz: 104.5e6, hiHz: 105.5e6 });
  assert.equal(ctx.store.get().live.retuneOffer, null, "no offer to press on a replay");
  await applyDeviceAction(ctx, retuneAction(105e6, "goto"));
  assert.deepEqual(calls, [], "not_live is decided before the request");
  assert.equal(ctx.store.get().toast.text, NOT_LIVE_TEXT);
});

test("T-343: the offer button says it moves the radio, names it, and sits on the edge the pan ran off", () => {
  assert.equal(retuneOfferModel(null, 100e6, "hackrf:x"), null);
  const lo = retuneOfferModel({ centerHz: 99e6 }, 100e6, "hackrf:abc")!;
  assert.equal(lo.label, "Retune to 99.0000 MHz");
  assert.match(lo.title, /Moves the radio on hackrf:abc/);
  assert.match(lo.title, /panning and zooming do not/);
  assert.equal(lo.belowBand, true);
  assert.equal(retuneOfferModel({ centerHz: 101e6 }, 100e6, null)!.belowBand, false);
  // An unidentified front end is not given a placeholder name (T-325's rule for device identity).
  assert.match(retuneOfferModel({ centerHz: 101e6 }, 100e6, null)!.title, /^Moves the radio\./);
});

test("T-343: a busy radio is reported, never retried into a race", () => {
  const e = new ControlError(409, "device_busy", "the front end (hackrf:abc) is busy: a retune has held it for 3.0 s");
  assert.match(retuneErrorText(e), /^The radio is busy: the front end \(hackrf:abc\) is busy/);
  assert.deepEqual(reactionTo(e).reaction, "busy");
});

/** The architectural half of the property: the gesture layer cannot reach a device route, and the
 * client's device routes have a known, small set of callers. A convention would drift; this does
 * not — adding a device call anywhere else fails here and has to be argued for. */
test("T-343: gestures.ts names no device route, and the device routes have exactly two callers", () => {
  const DEVICE_ROUTES = ["/api/control/center", "/api/control/rate", "/api/control/gains", "/api/control/bias_tee", "/api/control/baseband_filter"];
  const gestures = readFileSync("src/controls/gestures.ts", "utf8");
  for (const r of DEVICE_ROUTES) assert.ok(!gestures.includes(r), `gestures.ts must not name ${r}`);
  assert.ok(!/controls\/client|ControlClient|app\/context/.test(gestures), "gestures.ts must not reach the API client");

  const callers = walk("src")
    .filter((f) => DEVICE_ROUTES.some((r) => readFileSync(f, "utf8").includes(r)))
    .map((f) => f.slice("src/".length))
    .sort();
  // view.ts: the centre view's one retune path (Go to, a bookmark jump, an accepted edge offer).
  // review/device.ts: the SDR control panel, whose whole purpose is device settings.
  assert.deepEqual(callers, ["app/centre/view.ts", "app/review/device.ts"],
    "a new file reaches the front end: make it an explicit device action or route it through view.ts");
});

// ---- T-386: the overlay layer is not gated on a live stream socket -------------------------
//
// Every bracket, Confirmed band, selection box and frequency tick was placed in `live.view`, which
// exists only once a spectrum stream header has arrived. So a replay whose stream had not
// connected, a paused session, or a run whose stream ended rendered **no boxes at all**, silently,
// while `/api/inventory` was answering with rows for exactly the band the device reports — "we
// have it but didn't render it", with a single point of failure in front of it.

test("T-386 THE PROPERTY: with no stream header the overlays are still placed, in the tuned band the device reports", () => {
  const noHeader = { live: { view: null }, device: { centerHz: 101.3e6, sampleRateHz: 2e6 } };
  assert.deepEqual(centreView(noHeader), { loHz: 100.3e6, hiHz: 102.3e6 });
  // And a row of that band lands where the axis puts it, rather than nowhere.
  const v = centreView(noHeader)!;
  const bk = bracketLayout([makeCentreRow({ id: "e1", f_lo_hz: 101.2e6, f_hi_hz: 101.4e6, f_center_hz: 101.3e6 })], v, 1200, null);
  assert.deepEqual(bk.map((b) => b.id), ["e1"]);
  assert.ok(bk[0].leftPct > 44 && bk[0].leftPct < 46, `placed at ${bk[0].leftPct}%`);
  // The axis strip takes the same view, so brackets never draw over unlabelled ticks.
  assert.ok(tickModel(centreView(noHeader), 1200).length > 0, "the frequency axis is labelled too");
});

test("T-386 THE CONTROL: with neither a header nor a device the view stays UNKNOWN, never invented", () => {
  assert.equal(centreView({ live: { view: null }, device: { centerHz: null, sampleRateHz: null } }), null);
  assert.equal(centreView({ live: { view: null }, device: { centerHz: 101.3e6, sampleRateHz: null } }), null);
  // The header still wins when there is one: a zoom is the view, not the whole tuned band.
  assert.deepEqual(
    centreView({ live: { view: { loHz: 101e6, hiHz: 101.5e6 } }, device: { centerHz: 101.3e6, sampleRateHz: 2e6 } }),
    { loHz: 101e6, hiHz: 101.5e6 },
  );
});

test("T-386: the render path places overlays in centreView, and only the waterfall's own pass stays stream-gated", () => {
  const src = readFileSync("src/app/centre/live-spectrum.ts", "utf8");
  assert.match(src, /const s = store\.get\(\), v = centreView\(s\), g = geometryOfLive\(s\.live\)/);
  // The bail-out is now the view alone. Gating it on `g` as well is the bug: geometry needs a
  // header, and placement does not.
  assert.match(src, /\n\s*if \(!v\) \{/);
  assert.ok(!/if \(!v \|\| !g\) \{/.test(src), "the DOM overlays must not require the stream's bin geometry");
  // setBoxes is the exception, and says so: the render pass places boxes in texture units.
  assert.match(src, /if \(g\) \{\s*\n\s*wf\?\.setBoxes\(/);
  // And a device answer re-renders: it is a new view, and it does not arrive through `live.view`.
  // One key for both surfaces, so neither re-derives which inputs move the view.
  assert.match(src, /store\.select\(centreViewKey, schedule\)/);
  const axis = readFileSync("src/app/centre/axis-view.ts", "utf8");
  assert.match(axis, /tickModel\(centreView\(s\), el\.clientWidth\)/);
  assert.match(axis, /store\.select\(centreViewKey, schedule\)/);
  // The key must move when either input does, or a surface subscribing to it is subscribing to
  // nothing: a device-only change is exactly the case `live.view` alone misses.
  const noHeader = (centerHz: number) => centreViewKey({ live: { view: null }, device: { centerHz, sampleRateHz: 2e6 } });
  assert.notEqual(noHeader(101.3e6), noHeader(915e6));
  assert.equal(centreViewKey({ live: { view: null }, device: { centerHz: null, sampleRateHz: null } }), "");
});

test("T-386 CLOCK GUARD: no clock of the browser's own reaches the centre view modules", () => {
  // T-393's guard, extended to the modules T-386 touched. `live-spectrum.ts` formats *served*
  // capture times (`hms`) and must go on doing only that: the live edge, the review cursor and the
  // history window are all the capture clock's, and none of them may fall back to wall time.
  for (const f of ["src/app/centre/view.ts", "src/app/centre/axis-view.ts", "src/app/centre/live-spectrum.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset"]) {
      assert.ok(!src.includes(word), `${f} must not contain "${word}"`);
    }
  }
});

/** A `Row` for the centre-view placement tests; only the fields placement reads are meaningful. */
function makeCentreRow(over: Partial<Row> & { id: string }): Row {
  return {
    state: "candidate", f_center_hz: 100e6, bandwidth_hz: 200e3, f_lo_hz: 99.9e6, f_hi_hz: 100.1e6,
    first_seen_s: 0, last_seen_s: 0, count: 1, known_status: "unknown", status: null, tags: [],
    family: null, identity_scheme: null, identity_class: null, withheld: false, explanations: [],
    ...over,
  } as Row;
}

/** Every `.ts` file under `dir`, as paths relative to ui/ (tests run from there). */
function walk(dir: string): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = `${dir}/${e.name}`;
    if (e.isDirectory()) out.push(...walk(p));
    else if (e.name.endsWith(".ts")) out.push(p);
  }
  return out;
}
