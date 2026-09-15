// T-150 (ADR-0013 §3.3, §4.4, §8): capture-timeline pure helpers — activity-band reduction,
// scrub↔time mapping and the "reviewing N ago" / coverage wording. No DOM.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  DRAG_PX, WINDOW_S, agoText, coverageText, currentSpan, pctForAgo, reduceActivity, scrubToTime, selectionSpans, timeRegionName,
  timeWindowFromScrub, type HistoryGrid,
} from "../src/app/capture/timeline";

test("reduceActivity: empty grid and an all-gap grid are every column null", () => {
  assert.deepEqual(reduceActivity({ nf: 0, nt: 0, max_db: [] }, 4), [null, null, null, null]);
  assert.deepEqual(reduceActivity({ nf: 2, nt: 2, max_db: [null, null, null, null] }, 2), [null, null]);
});

test("reduceActivity: a column's value is the max over every frequency bin in its time share", () => {
  // One time row, two frequency bins, one column: the column covers both, so its value is their max.
  assert.deepEqual(reduceActivity({ nf: 2, nt: 1, max_db: [-90, -60] }, 1), [1]);
});

test("reduceActivity: columns are chronological and normalised against the grid's own range", () => {
  const grid: HistoryGrid = { nf: 1, nt: 2, max_db: [-90, -60] };
  assert.deepEqual(reduceActivity(grid, 2), [0, 1]);
});

test("reduceActivity: a column with no observed cell is null even when other columns have data", () => {
  const grid: HistoryGrid = { nf: 1, nt: 2, max_db: [-80, null] };
  assert.deepEqual(reduceActivity(grid, 2), [0, null]);
});

test("scrubToTime: live past 98.5%, otherwise ago maps linearly over the window", () => {
  const now = 1_800_000_000;
  assert.deepEqual(scrubToTime(100, now, WINDOW_S), { live: true, tS: now, agoS: 0 });
  assert.deepEqual(scrubToTime(99, now, WINDOW_S), { live: true, tS: now, agoS: 0 });
  const mid = scrubToTime(50, now, 100);
  assert.equal(mid.live, false);
  assert.equal(mid.agoS, 50);
  assert.equal(mid.tS, now - 50);
  // out of range clamps
  assert.equal(scrubToTime(-10, now, 100).agoS, 100);
  assert.equal(scrubToTime(200, now, 100).live, true);
});

test("pctForAgo is scrubToTime's inverse at the sampled points", () => {
  assert.equal(pctForAgo(0, 100), 100);
  assert.equal(pctForAgo(100, 100), 0);
  assert.equal(pctForAgo(50, 100), 50);
});

test("agoText: minutes under an hour, hours to one decimal above", () => {
  assert.equal(agoText(30), "1 min");
  assert.equal(agoText(59 * 60), "59 min");
  assert.equal(agoText(90 * 60), "1.5 h");
  assert.equal(agoText(3 * 3600), "3.0 h");
});

test("coverageText: honest interim, no invented buffered-hours/bytes numbers", () => {
  assert.equal(coverageText(null), "coverage unknown");
  assert.equal(coverageText(0.992), "99% of the last 48 h observed");
  assert.equal(coverageText(0), "0% of the last 48 h observed");
  assert.doesNotMatch(coverageText(0.5), /GB|buffered/);
});

test("currentSpan prefers the live geometry, falls back to the tuned device span, else null", () => {
  assert.deepEqual(
    currentSpan({ live: { loHz: 99_600_000, hiHz: 102_000_000 }, device: { centerHz: null, sampleRateHz: null } }),
    { loHz: 99_600_000, hiHz: 102_000_000 },
  );
  assert.deepEqual(
    currentSpan({ live: null, device: { centerHz: 100_800_000, sampleRateHz: 2_400_000 } }),
    { loHz: 99_600_000, hiHz: 102_000_000 },
  );
  assert.equal(currentSpan({ live: null, device: { centerHz: null, sampleRateHz: null } }), null);
});

// ---- time-window select (T-194) ----

test("timeWindowFromScrub: chronologically ordered, null when both ends resolve to the same instant", () => {
  const now = 1_800_000_000;
  assert.deepEqual(timeWindowFromScrub(80, 50, now, 100), { t_lo: now - 50, t_hi: now - 20 });
  assert.deepEqual(timeWindowFromScrub(50, 80, now, 100), { t_lo: now - 50, t_hi: now - 20 }, "order-independent");
  assert.equal(timeWindowFromScrub(99, 100, now, 100), null, "both past the LIVE threshold");
  assert.equal(timeWindowFromScrub(40, 40, now, 100), null, "no width");
  assert.equal(DRAG_PX, 6);
});

test("timeRegionName: the UTC wall-clock span, to the second", () => {
  const t0 = Date.UTC(2026, 8, 15, 12, 34, 56) / 1000, t1 = Date.UTC(2026, 8, 15, 12, 35, 10) / 1000;
  assert.equal(timeRegionName(t0, t1), "12:34:56Z–12:35:10Z");
});

test("selectionSpans: placed by the inverse of scrubToTime; no window, or wholly outside, is omitted", () => {
  const list = [
    { id: "recent", t_lo: 900, t_hi: 950 }, // 100..50 s ago
    { id: "no-window" },
    { id: "future", t_lo: 1000, t_hi: 1010 }, // clamps to a single point at the LIVE edge
    { id: "too-old", t_lo: 800, t_hi: 850 }, // 200..150 s ago, past the retained window
  ];
  const spans = selectionSpans(list, 1000, 100);
  assert.deepEqual(spans, [{ id: "recent", leftPct: 0, widthPct: 50 }]);
});

// ---- layout: the scrubbable band stays touch-usable and full-width at narrow widths ----

test("capture.css: the timeline is fluid (no fixed wide pixel width) and scrubbable by touch", () => {
  const css = readFileSync("src/app/capture/capture.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  // the 2px playhead line is a decoration, not a layout width; anything wider would be a bug.
  for (const m of css.matchAll(/(?<!min-)\bwidth:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 4, `unexpected fixed width: ${m[0]}`);
  assert.match(css, /\.cap-band\s*\{[^}]*touch-action:\s*none/, "pointer events, not native scroll, drive the scrub drag");
  // base.css gives the capture row a fixed height at every breakpoint down to 900px, so the band
  // never collapses to nothing when the page goes to one column.
  assert.match(readFileSync("src/app/base.css", "utf8"), /\.centre\s*\{[^}]*grid-template-rows:[^}]*92px/);
});
