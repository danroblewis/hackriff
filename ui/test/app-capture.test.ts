// T-150 (ADR-0013 §3.3, §4.4, §8): capture-timeline pure helpers — scrub↔time mapping, the
// "reviewing N ago" / coverage wording, and shading a grid the backend measured. No DOM.
//
// T-338: there is no `WINDOW_S` any more. Every helper takes the capture window's span, and the
// span comes from `GET /api/timeline` (the IQ ring's configured retention). The test at the bottom
// of this file is the guard: a default would let the band silently size itself from a constant
// again, which is exactly the failure the user's invariant names.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  DRAG_PX, MAX_MARKS, MIN_MARK_PCT, agoText, bufferedSpan, captureWindow, coverageText, currentSpan, durationText,
  eventMarkTitle, eventMarks, inCoverageGap, iqBackingAt, observedFraction, overviewShade, pctForAgo,
  scrubDataNote, scrubToTime, selectionSpans, timeRegionName, timeWindowFromScrub,
  type CaptureWindow, type EventRow,
} from "../src/app/capture/timeline";

/** A capture window as `GET /api/timeline` reports it. */
const winOf = (t1S: number, spanS: number, buffered: { t0S: number; t1S: number } | null = null): CaptureWindow =>
  ({ t0S: t1S - spanS, t1S, spanS, buffered });

test("captureWindow: the band is the ring's retention, and an incomplete window is unknown", () => {
  // The invariant, at the client's edge. The span is whatever the backend reported, so a
  // reconfigured retention moves the band; anything missing is `null`, never a default span.
  const w = captureWindow({ window: { t0_s: 900, t1_s: 1000, span_s: 100, buffered: { t0_s: 970, t1_s: 1000 } } });
  assert.deepEqual(w, { t0S: 900, t1S: 1000, spanS: 100, buffered: { t0S: 970, t1S: 1000 } });
  assert.equal(captureWindow({ window: { t0_s: 900, t1_s: 1000, span_s: 3600, buffered: null } })!.spanS, 3600,
    "a longer retention is a longer band, not a rescaled one");
  assert.equal(captureWindow(null), null, "not answered yet");
  assert.equal(captureWindow({ window: null }), null, "no capture window on this server");
  assert.equal(captureWindow({ window: { t0_s: null, t1_s: null, span_s: null } }), null, "no live edge yet");
  assert.equal(captureWindow({ window: { t0_s: 900, t1_s: 1000, span_s: 0 } }), null, "a zero retention is no band");
});

test("bufferedSpan: what the ring holds sits inside the band and never resizes it", () => {
  // A ring part-way through filling covers part of its retention. That difference is the thing the
  // track exists to show, so it is placed against the band, not used to shorten it.
  assert.deepEqual(bufferedSpan(winOf(1000, 100, { t0S: 950, t1S: 1000 })), { id: "ring", leftPct: 50, widthPct: 50 });
  assert.deepEqual(bufferedSpan(winOf(1000, 100, { t0S: 900, t1S: 1000 })), { id: "ring", leftPct: 0, widthPct: 100 });
  assert.equal(bufferedSpan(winOf(1000, 100, null)), null, "a ring holding nothing is unknown, not empty");
  assert.equal(bufferedSpan(null), null, "not answered yet");
});

test("overviewShade: normalised against the range the backend measured, not against the values in hand", () => {
  // Deciding a band's dynamic range from whatever came back is a measurement. With a served range
  // the shade is arithmetic; without one there is nothing measured to shade against.
  const grid = { nt: 2, nf: 1, max_db: [-100, -60], range_db: { lo: -100, hi: -60 } };
  assert.equal(overviewShade(grid, 0), 0);
  assert.equal(overviewShade(grid, 1), 1);
  // the same values against a wider measured range shade differently — proof the range is used
  assert.equal(overviewShade({ ...grid, range_db: { lo: -100, hi: -20 } }, 1), 0.5);
  assert.equal(overviewShade({ ...grid, range_db: null }, 1), null, "no measured range, no shading");
  assert.equal(overviewShade({ nt: 1, nf: 1, max_db: [null], range_db: { lo: -100, hi: -60 } }, 0), null,
    "an unobserved cell is null, never the low end of the scale");
});

test("observedFraction comes from the grid's own counts, and unknown stays unknown", () => {
  assert.equal(observedFraction({ nt: 4, nf: 2, max_db: [], cells: 8, observed_cells: 2 }), 0.25);
  assert.equal(observedFraction(null), null, "not asked yet is never 'nothing was observed'");
  assert.equal(observedFraction({ nt: 4, nf: 2, max_db: [] }), null, "counts absent is unknown");
});

test("scrubToTime: live past 98.5%, otherwise ago maps linearly over the window", () => {
  const now = 1_800_000_000;
  assert.deepEqual(scrubToTime(100, now, 48 * 3600), { live: true, tS: now, agoS: 0 });
  assert.deepEqual(scrubToTime(99, now, 48 * 3600), { live: true, tS: now, agoS: 0 });
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

test("coverageText names the capture window it measured, never a constant", () => {
  assert.equal(coverageText(null, 120), "coverage unknown");
  assert.equal(coverageText(0.5, null), "coverage unknown", "no window is unknown, not 50% of nothing");
  assert.equal(coverageText(0.992, 60), "99% of the retained 60 s observed");
  assert.equal(coverageText(0, 3600), "0% of the retained 1.0 h observed");
  // the wording follows the configured retention: a reconfigured ring reads differently
  assert.notEqual(coverageText(0.5, 60), coverageText(0.5, 3600));
  assert.doesNotMatch(coverageText(0.5, 60), /GB|buffered|48 h/);
});

test("durationText keeps seconds: a 90 s retention is not rounded into minutes", () => {
  assert.equal(durationText(45), "45 s");
  assert.equal(durationText(89), "89 s");
  assert.equal(durationText(120), "2 min");
  assert.equal(durationText(300), "5 min");
  assert.equal(durationText(3600), "1.0 h");
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

// ---- past events on the scrubber (T-263, ADR-0017 TM-7) ----

/** A row as `/api/inventory` serves it, reduced to the fields the marks are placed from. */
function evRow(over: Partial<EventRow> = {}): EventRow {
  return { id: "e1", state: "confirmed", presence: { last_interval: null }, recurrence: { recent: [] }, ...over };
}

test("eventMarks: one mark per distinct timespan, from presence.last_interval and recurrence.recent alike", () => {
  const now = 1000;
  const rows = [evRow({
    presence: { last_interval: { t_start_s: 950, t_end_s: 960 } },
    recurrence: { recent: [{ t_start_s: 900, t_end_s: 910 }, { t_start_s: 950, t_end_s: 960 }] },
  })];
  const marks = eventMarks(rows, now, 100);
  assert.equal(marks.length, 2, "the interval that repeats an appearance is not drawn twice");
  assert.deepEqual(marks.map((m) => [m.tStartS, m.tEndS]), [[900, 910], [950, 960]], "newest last");
  assert.deepEqual(marks.map((m) => [m.leftPct, m.widthPct]), [[0, 10], [50, 10]], "placed by pctForAgo");
});

test("eventMarks: a one-off burst keeps its measured timespan and is still drawn (ADR-0017 §1.2)", () => {
  // Milliseconds against a 48 h band round to no width at all. The event is the whole point of the
  // time model, so it is drawn at the floor — and the floor is a drawing width, never a duration:
  // tStartS/tEndS keep what was measured, and eventMarkTitle reports the real 40 ms.
  const now = 1_800_000_000;
  const [m] = eventMarks([evRow({ presence: { last_interval: { t_start_s: now - 3600, t_end_s: now - 3599.96 } } })], now, 48 * 3600);
  assert.equal(m.widthPct, MIN_MARK_PCT);
  assert.ok(Math.abs(m.tEndS - m.tStartS - 0.04) < 1e-6, "the measured timespan is carried through untouched");
  assert.match(eventMarkTitle(m, now), /lasted 40 ms/, "the title reports the measurement, not the drawn width");
});

test("eventMarks: a timespan outside the retained window is dropped, never clamped to the band's edge", () => {
  // Clamping would place an event at a time it did not happen — the worst failure a scrubber can
  // have, because it teaches the user to distrust the marks and the list together.
  const now = 1000;
  const old = evRow({ id: "old", presence: { last_interval: { t_start_s: 700, t_end_s: 800 } } });
  const future = evRow({ id: "future", presence: { last_interval: { t_start_s: 1100, t_end_s: 1200 } } });
  assert.deepEqual(eventMarks([old, future], now, 100), []);
  // one that straddles the window's start is kept: part of it did happen inside the band.
  const straddling = evRow({ presence: { last_interval: { t_start_s: 850, t_end_s: 950 } } });
  assert.equal(eventMarks([straddling], now, 100).length, 1);
});

test("eventMarks: rows in neither list are skipped, and the render cap keeps the newest", () => {
  const now = 1000;
  assert.deepEqual(eventMarks([evRow({ state: "deleted", presence: { last_interval: { t_start_s: 950, t_end_s: 960 } } })], now, 100), []);
  const many = evRow({
    recurrence: { recent: Array.from({ length: MAX_MARKS + 10 }, (_, i) => ({ t_start_s: 900 + i * 0.01, t_end_s: 900 + i * 0.01 })) },
  });
  const marks = eventMarks([many], now, 100);
  assert.equal(marks.length, MAX_MARKS);
  assert.equal(marks[marks.length - 1].tEndS, 900 + (MAX_MARKS + 9) * 0.01, "the newest survive the cap");
});

test("eventMarks carries no liveness: an appearance never measured one", () => {
  const [m] = eventMarks([evRow({ presence: { last_interval: { t_start_s: 950, t_end_s: 1000 } } })], 1000, 100);
  assert.ok(!("open" in m) && !("liveness" in m), "a mark is a timespan; liveness is the list's answer");
});

// ---- what backs a scrub-back: no data vs nothing on air ----

test("iqBackingAt / inCoverageGap: three distinct answers, and unknown is one of them", () => {
  const ring = winOf(1000, 3600, { t0S: 900, t1S: 1000 });
  assert.equal(iqBackingAt(1000, true, ring), "live");
  assert.equal(iqBackingAt(950, false, ring), "ring");
  assert.equal(iqBackingAt(500, false, ring), "outside-ring");
  assert.equal(iqBackingAt(950, false, null), "unknown", "an unanswered window is not 'no ring'");
  assert.equal(inCoverageGap(950, null), null, "no coverage summary yet is unknown, not observed");
  assert.equal(inCoverageGap(950, [{ t0_s: 940, t1_s: 960 }]), true);
  assert.equal(inCoverageGap(970, [{ t0_s: 940, t1_s: 960 }]), false);
});

test("scrubDataNote: an unobserved window never reads as a quiet band", () => {
  // The trap this exists for: an empty list over a window nobody listened to is not a measurement
  // that nothing was transmitting. The two claims lead a user to act differently.
  const ring = winOf(1000, 3600, { t0S: 900, t1S: 1000 });
  const gapNote = scrubDataNote(950, false, ring, [{ t0_s: 940, t1_s: 960 }]);
  assert.match(gapNote, /no data for this window/);
  // every mention of quiet in the sentence is a negated one: the note denies the reading, and
  // nowhere offers it. A note that said only "no data" would still leave "quiet" available.
  assert.match(gapNote, /not a quiet band$/);
  for (const at of [...gapNote.matchAll(/quiet/g)].map((x) => x.index)) {
    assert.ok(gapNote.slice(0, at).endsWith("not a "), `unnegated "quiet" at ${at}: ${gapNote}`);
  }
  // and the unobserved answer wins over the ring's own coverage, which would otherwise read as fine
  assert.notEqual(gapNote, scrubDataNote(950, false, ring, []));
});

test("scrubDataNote: live says nothing extra; the ring, past the ring, and unknown are distinct", () => {
  const ring = winOf(1000, 3600, { t0S: 900, t1S: 1000 });
  assert.equal(scrubDataNote(1000, true, ring, []), "", "the band's own coverage text speaks for the live edge");
  assert.match(scrubDataNote(950, false, ring, []), /IQ retained/);
  assert.match(scrubDataNote(500, false, ring, []), /past the IQ ring/);
  assert.match(scrubDataNote(500, false, ring, []), /stored history/, "the lists still answer past the ring");
  assert.match(scrubDataNote(950, false, null, []), /unknown/);
  const notes = [scrubDataNote(950, false, ring, []), scrubDataNote(500, false, ring, []), scrubDataNote(950, false, null, [])];
  assert.equal(new Set(notes).size, 3, "three different situations never share one sentence");
});

test("the band's span has no default: nothing can fall back to a constant window", () => {
  // The regression this file exists to prevent. `WINDOW_S = 48 * 3600` used to size the scrubber,
  // which let it offer times the IQ ring had already overwritten. Every helper must require the
  // span, and no RF or retention constant may live in this module.
  const src = readFileSync("src/app/capture/timeline.ts", "utf8");
  assert.doesNotMatch(src, /windowS\s*:\s*number\s*=/, "a defaulted windowS is a constant in disguise");
  assert.doesNotMatch(src, /(export )?const WINDOW_S/, "the constant itself is gone, not merely unused");
  assert.doesNotMatch(src.replace(/\/\*[\s\S]*?\*\/|\/\/.*/g, ""), /48\s*\*\s*3600/);
});

test("capture.css: the marks layer, ring track and overview canvas are drawn, and none is fixed-width", () => {
  const css = readFileSync("src/app/capture/capture.css", "utf8");
  assert.match(css, /\.cap-overview\s*\{/, "the band is a data display, not an empty box");
  assert.match(css, /image-rendering:\s*pixelated/, "upscaling repeats measured cells, never interpolates");
  assert.match(css, /\.cap-mark\s*\{/);
  assert.match(css, /\.cap-mark\.confirmed\s*\{/, "confirmed and candidate marks are told apart");
  assert.match(css, /\.cap-ring\s*\{/);
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
