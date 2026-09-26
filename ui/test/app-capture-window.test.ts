// The capture window on the canvas's time axis (T-506). These are the invariants the retired
// Capture panel's suite (`app-capture.test.ts`, T-150/T-263/T-338/T-386) held, re-pointed at the
// code that now carries them: `app/centre/capture-window.ts` (pure), `capture-clock.ts` (the poll
// and Record IQ) and `surface.ts` (where the rules are drawn and the extent is floored).
//
// What moved, what was deleted, and why:
//  - MOVED: the window is the ring's retention (captureWindow); what the ring holds sits inside it
//    and never resizes it (bufferedSpan → ringRules); the three IQ-backing answers (iqBackingAt);
//    the one-clause note (scrubDataNote → iqNote); no default span; the T-386 clock guard.
//  - DELETED with the band they described: scrub↔percent mapping, the overview shading and its
//    T-342 guards, event marks on the band, selection spans on the band, the time-window drag, the
//    T-386 band-key tagging, the T-391 collapse layout. The canvas draws the energy through the one
//    cell rule, the signal boxes and selections through `marks.ts`, and region selection is its
//    shift+drag (T-458) — each already under its own suite.
//  - DELETED: scrubDataNote's coverage-gap clause. On the canvas an unobserved stretch is drawn as
//    unobserved (grey) by the one cell rule, cell by cell, so a sentence restating it for the whole
//    window is no longer the only place the distinction is made (`surface-honesty.test.ts`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
import {
  DROP_LEAD_S, IQ_RULE_INK, RETENTION_RULE_INK, captureWindow, currentSpan, durationText, iqAvailability,
  iqBackingAt, iqNote, ringRuleQuads, ringRules, type CaptureWindow,
} from "../src/app/centre/capture-window";
import {
  CAPTURE_CLOCK_MS, CAPTURE_CLOCK_REQUEST, IQ_AVAILABILITY_REQUEST, frozenRecordNote, recordLabel, recordTitle,
} from "../src/app/centre/capture-clock";
import { timeRuleQuads } from "../src/surface/marks";
import { quadSizePx } from "../src/surface/minimap";
import { timeExtent } from "../src/navigators";

/** A capture window as `GET /api/timeline` reports it. */
const winOf = (t1S: number, spanS: number, buffered: { t0S: number; t1S: number } | null = null): CaptureWindow =>
  ({ t0S: t1S - spanS, t1S, spanS, buffered });

const S = 1e9;
const RECT = { x: 0, y: 0, w: 800, h: 400 };
const boxOf = (t0S: number, t1S: number) => ({ f0Hz: 99e6, f1Hz: 101e6, t0Ns: t0S * S, t1Ns: t1S * S });
/** The clip-space centre y of a horizontal rule quad. */
const midY = (q: { clip: readonly number[] }) => (q.clip[1] + q.clip[3]) / 2;
const clipYOf = (tS: number, box: { t0Ns: number; t1Ns: number }) => (2 * (tS * S - box.t0Ns)) / (box.t1Ns - box.t0Ns) - 1;

test("captureWindow: the window is the ring's retention, and an incomplete window is unknown", () => {
  const w = captureWindow({ window: { t0_s: 900, t1_s: 1000, span_s: 100, buffered: { t0_s: 970, t1_s: 1000 } } });
  assert.deepEqual(w, { t0S: 900, t1S: 1000, spanS: 100, buffered: { t0S: 970, t1S: 1000 } });
  assert.equal(captureWindow({ window: { t0_s: 900, t1_s: 1000, span_s: 3600, buffered: null } })!.spanS, 3600,
    "a longer retention is a longer window, not a rescaled one");
  assert.equal(captureWindow(null), null, "not answered yet");
  assert.equal(captureWindow({ window: null }), null, "no capture window on this server");
  assert.equal(captureWindow({ window: { t0_s: null, t1_s: null, span_s: null } }), null, "no live edge yet");
  assert.equal(captureWindow({ window: { t0_s: 900, t1_s: 1000, span_s: 0 } }), null, "a zero retention is no window");
});

test("ringRules: what the ring holds sits inside the window and never resizes it", () => {
  // A ring part-way through filling covers part of its retention. The retention bound stays at the
  // configured span; the IQ horizon is where the ring actually starts. That difference is the thing
  // the two rules exist to show.
  const filling = ringRules(winOf(1000, 100, { t0S: 950, t1S: 1000 }), 1000)!;
  assert.equal(filling.retentionS, 900, "the retention bound is edge − retention_s, whatever the ring holds");
  assert.equal(filling.iqS, 950, "the IQ horizon is the oldest sample the ring holds");
  const full = ringRules(winOf(1000, 100, { t0S: 900, t1S: 1000 }), 1000)!;
  assert.equal(full.retentionS, 900);
  assert.equal(full.iqS, 900, "a full ring's IQ horizon IS its retention bound");
  assert.equal(ringRules(winOf(1000, 100, null), 1000)!.iqS, null, "a ring holding nothing is unknown, not empty");
  assert.equal(ringRules(null, 1000), null, "not answered yet: no rules, never a default window");
});

test("ringRules: both rules advance with the edge the panes are drawn to, not on the poll", () => {
  // The one-shared-time-axis rule: the rows move per frame, so a rule placed only on the 5 s poll
  // would drift against them and then jump. The edge handed in is the capture clock's, per frame.
  const w = winOf(1000, 100, { t0S: 900, t1S: 1000 });
  const later = ringRules(w, 1003)!;
  assert.equal(later.retentionS, 903, "the retention bound moves with the edge");
  assert.equal(later.iqS, 903, "a full ring's horizon moves with it — never claims IQ the ring evicted");
  // An edge older than the window's own (a stream that has not caught up) never drags it backwards.
  assert.equal(ringRules(w, 990)!.retentionS, 900);
  assert.equal(ringRules(w, null)!.retentionS, 900, "no edge yet: the window's own t1");
  // A filling ring's horizon is a fixed instant; the edge moving does not move it.
  assert.equal(ringRules(winOf(1000, 100, { t0S: 950, t1S: 1000 }), 1010)!.iqS, 950);
});

/**
 * A byte-full ring as `hk_store::iqbuffer` runs one (T-845): 16 slots of `slotS` each, a whole slot
 * evicted the moment the writer crosses into a new one, so the oldest sample jumps a slot at a time
 * and holds between 15 and 16 slots — AHEAD of the retention bound for most of each slot. `drops` is
 * the schedule the server states at writer time `wS` (`buffered.drops`, lookahead 60 s).
 */
function fullRing(slotS: number, slots = 16) {
  const t0At = (wS: number) => (Math.floor(wS / slotS) - (slots - 1)) * slotS;
  return {
    t0At,
    window: (wS: number, withDrops: boolean): CaptureWindow => {
      const drops = [];
      for (let k = Math.floor(wS / slotS) + 1; k * slotS <= wS + 60 + slotS; k++) drops.push({ atS: k * slotS, t0S: t0At(k * slotS) });
      const retention = slots * slotS;
      return { t0S: wS - retention, t1S: wS, spanS: retention, buffered: { t0S: t0At(wS), t1S: wS, ...(withDrops ? { drops } : {}) } };
    },
  };
}

test("T-845: the drawn IQ horizon is never older than what the ring holds, across whole-slot drops", () => {
  // The observed defect: the page re-polls the ring window every CAPTURE_CLOCK_MS (5 s) and the ring
  // drops 7.5 s slots, so for up to one poll after a drop the horizon drawn per frame claimed up to
  // 7.5 s of IQ already gone. Frame by frame (count-based: every 16 ms of capture over 120 s), with
  // the edge the panes are drawn to lagging the ring's writer by up to 400 ms, the horizon from the
  // LAST poll must never be older than the ring's true oldest sample at that frame.
  const pollS = CAPTURE_CLOCK_MS / 1000;
  const slot = 7.5, start = 10_000;
  const ring = fullRing(slot);
  let checked = 0, drops = 0, worstOld = -Infinity, slack = 0, exact = 0;
  for (const lagS of [0, 0.05, 0.4]) {
    let polled = ring.window(start, true), stale = ring.window(start, false), lastPoll = start;
    let prevT0 = ring.t0At(start);
    for (let i = 0; i * 0.016 < 120; i++) {
      const wS = start + i * 0.016;
      if (wS - lastPoll >= pollS) { polled = ring.window(wS, true); stale = ring.window(wS, false); lastPoll = wS; }
      const truth = ring.t0At(wS);
      if (truth > prevT0) { drops++; prevT0 = truth; }
      const edge = wS - lagS;
      const iq = ringRules(polled, edge)!.iqS!;
      assert.ok(iq >= truth - 1e-9, `frame ${i} (lag ${lagS} s): horizon ${iq} claims IQ older than the ring's oldest ${truth}`);
      assert.ok(iq <= truth + slot + 1e-9, `the horizon under-promises by more than one slot: ${iq} vs ${truth}`);
      slack = Math.max(slack, iq - truth);
      if (Math.abs(iq - truth) < 1e-9) exact++;
      worstOld = Math.max(worstOld, truth - ringRules(stale, edge)!.iqS!);
      checked++;
    }
  }
  assert.ok(checked > 20_000 && drops >= 45, `must span many drops: ${checked} frames, ${drops} drops`);
  // …and it under-promises by at most one slot, only within DROP_LEAD_S of a drop: for most of
  // each slot it is exactly the ring's oldest sample.
  assert.ok(slack <= slot + 1e-9, `under-promised by ${slack} s`);
  assert.ok(exact / checked > 1 - (DROP_LEAD_S + 0.5) / slot, `exactly right on only ${exact} of ${checked} frames`);
  // The control: the same polls without the schedule draw the pre-T-845 horizon, which DOES claim
  // dropped IQ — by up to a whole poll's worth of slot.
  assert.ok(worstOld > 1, `without drops the horizon should have claimed dropped IQ; worst ${worstOld} s`);
});

test("T-845: captureWindow parses buffered.drops, and a malformed or disordered list is no list", () => {
  const w = captureWindow({ window: { t0_s: 880, t1_s: 1000, span_s: 120, buffered: { t0_s: 885, t1_s: 1000,
    drops: [{ at_s: 1001.5, t0_s: 887.5 }, { at_s: 1010, t0_s: 895 }] } } })!;
  assert.deepEqual(w.buffered, { t0S: 885, t1S: 1000, drops: [{ atS: 1001.5, t0S: 887.5 }, { atS: 1010, t0S: 895 }] });
  const bad = captureWindow({ window: { t0_s: 880, t1_s: 1000, span_s: 120, buffered: { t0_s: 885, t1_s: 1000,
    drops: [{ at_s: 1010, t0_s: 895 }, { at_s: 1002.5, t0_s: 887.5 }] } } })!;
  assert.deepEqual(bad.buffered!.drops, [], "a disordered schedule is not trusted");
  // The rule: the next pending drop is applied ahead of its time; past the schedule, its last.
  assert.equal(ringRules(w, 1000)!.iqS, 887.5, "a drop within the lead is honoured ahead of its time");
  assert.equal(ringRules(w, 1000)!.dropT0S, 887.5);
  assert.equal(ringRules(w, 1005)!.iqS, 887.5, "a drop beyond the lead is not");
  assert.equal(ringRules(w, 1008)!.iqS, 895);
  assert.equal(ringRules(w, 1020)!.iqS, 900, "past every drop: the last, or the retention bound if newer");
  // A FILLING ring whose first drop is far off draws exactly what it holds, never a slot less.
  const filling = captureWindow({ window: { t0_s: 880, t1_s: 1000, span_s: 120, buffered: { t0_s: 950, t1_s: 1000,
    drops: [{ at_s: 1070, t0_s: 957.5 }] } } });
  assert.equal(ringRules(filling, 1000)!.iqS, 950);
  assert.equal(ringRules(filling, 1000)!.dropT0S, null);
  assert.equal(ringRules(filling, 1068.5)!.iqS, 957.5, "…until the drop comes within the lead");
  // A ring whose retention trims to the sample schedules nothing: the retention rule alone, as before.
  assert.equal(ringRules(captureWindow({ window: { t0_s: 880, t1_s: 1000, span_s: 120, buffered: { t0_s: 885, t1_s: 1000, drops: [] } } }), 1000)!.iqS, 885);
});

test("iqBackingAt: live, in the ring, past the ring, and unknown are four different answers", () => {
  const rules = ringRules(winOf(1000, 3600, { t0S: 900, t1S: 1000 }), 1000);
  assert.equal(iqBackingAt(1000, true, rules), "live");
  assert.equal(iqBackingAt(950, false, rules), "ring");
  assert.equal(iqBackingAt(500, false, rules), "outside-ring");
  assert.equal(iqBackingAt(950, false, null), "unknown", "an unanswered window is not 'no ring'");
  assert.equal(iqBackingAt(950, false, ringRules(winOf(1000, 100, null), 1000)), "unknown", "an empty ring is unknown");
});

test("iqNote: past the ring promises no audio, and no two situations share a sentence", () => {
  const notes = (["live", "ring", "recording", "outside-ring", "unknown"] as const).map(iqNote);
  assert.equal(new Set(notes).size, 5);
  assert.match(iqNote("outside-ring"), /no IQ and no audio/, "promising audio that cannot be delivered is the defect");
  assert.match(iqNote("outside-ring"), /spectrum history only/, "the waterfall still answers past the ring");
  assert.match(iqNote("ring"), /demod and decode can re-run/);
  assert.match(iqNote("recording"), /demod and decode can re-run/, "a recording extends the horizon exactly like the ring");
  assert.match(iqNote("unknown"), /unknown/);
});

// ---- T-464: the wider horizon — ring AND recordings, never spectrum coverage ----

test("iqAvailability: parses GET /api/recordings' iq_available.spans, dropping anything malformed", () => {
  const r = {
    iq_available: {
      spans: [
        { t0: 900, t1: 1000, t0_ns: 900e9, t1_ns: 1000e9, span_s: 100, source: "ring", recording: null },
        { t0: 500, t1: 600, t0_ns: 500e9, t1_ns: 600e9, span_s: 100, source: "recording", recording: "rec-1" },
        // Dropped: inverted, unknown source, missing fields.
        { t0: 700, t1: 600, source: "ring", recording: null },
        { t0: 100, t1: 200, source: "coverage", recording: null },
        { t0: 100, source: "ring" },
      ],
    },
  };
  assert.deepEqual(iqAvailability(r), [
    { t0S: 900, t1S: 1000, source: "ring", recording: null },
    { t0S: 500, t1S: 600, source: "recording", recording: "rec-1" },
  ]);
  assert.deepEqual(iqAvailability(null), [], "no response is no spans, not a guess");
  assert.deepEqual(iqAvailability({}), [], "an old server with no iq_available at all");
  assert.deepEqual(iqAvailability({ iq_available: { spans: null } }), []);
});

test("iqBackingAt: a recording past the ring answers 'recording', never 'outside-ring' (T-464's whole point)", () => {
  const rules = ringRules(winOf(1000, 3600, { t0S: 900, t1S: 1000 }), 1000);
  const spans = [{ t0S: 300, t1S: 500, source: "recording" as const, recording: "rec-9" }];
  assert.equal(iqBackingAt(950, false, rules, spans), "ring");
  assert.equal(iqBackingAt(400, false, rules, spans), "recording", "the ring alone would have called this outside-ring");
  assert.equal(iqBackingAt(600, false, rules, spans), "outside-ring", "the genuine hole between the ring and the recording");
  assert.equal(iqBackingAt(1000, true, rules, spans), "live");
  assert.equal(iqBackingAt(400, false, rules, []), "outside-ring", "answered, and nothing extends the horizon there");
  // No recordings poll running yet (`null`): falls back to the ring-only answer — never silently
  // claims a recording nobody asked about.
  assert.equal(iqBackingAt(950, false, rules, null), "ring");
  assert.equal(iqBackingAt(400, false, rules, null), "outside-ring");
});

test("REVIEW FIX: the ring answer is the FRESH per-frame `rules`, never the polled `spans` snapshot", () => {
  // Case 1 (review finding): a pane paused right at the live edge, a moment after a 5 s-old poll,
  // must not read "outside-ring" just because the polled ring span's upper bound is now stale — the
  // ring has no upper bound short of live, and `rules` (recomputed this frame from the edge the
  // panes are actually drawn to) already knows that.
  const rulesAtEdge = ringRules(winOf(1000, 3600, { t0S: 900, t1S: 1000 }), 1004)!;
  const staleRingSpan = [{ t0S: 900, t1S: 1000, source: "ring" as const, recording: null }];
  const staleCoveredByAlone = staleRingSpan.some((s) => 1004 >= s.t0S && 1004 <= s.t1S);
  assert.equal(staleCoveredByAlone, false, "the stale span's own upper bound would have rejected this instant");
  assert.equal(iqBackingAt(1004, false, rulesAtEdge, staleRingSpan), "ring",
    "the fresh rules cover it even though a spans-only check would not");

  // Case 2 (review finding): the ring has rolled forward since the last poll (a FULL ring's horizon
  // moves with the edge), so a position the stale poll still claimed must not be answered "ring" —
  // that promises audio `/api/playback` has already stopped being able to deliver.
  const rolledRules = ringRules(winOf(1000, 100, { t0S: 900, t1S: 1000 }), 1010)!; // full ring, 10 s later: iqS = 910
  assert.equal(rolledRules.iqS, 910);
  const stillStaleSpan = [{ t0S: 900, t1S: 1000, source: "ring" as const, recording: null }]; // polled before it rolled
  assert.equal(iqBackingAt(905, false, rolledRules, stillStaleSpan), "outside-ring",
    "the stale span still claims 900..1000; the fresh rules know the true horizon moved to 910");
});

test("T-464 GUARD: the IQ-available horizon comes from GET /api/recordings, never from coverage or tiles", () => {
  // The trap the ticket names by name: this is a DIFFERENT question from the coverage map's
  // "was this observed" — conflating them would promise audio the front end cannot deliver.
  assert.equal(IQ_AVAILABILITY_REQUEST, "/api/recordings");
  assert.doesNotMatch(IQ_AVAILABILITY_REQUEST, /coverage|tiles/);
  const noComments = (f: string) =>
    readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  const src = noComments("src/app/centre/capture-window.ts");
  assert.doesNotMatch(src, /api\/coverage|api\/tiles|CoverageCell|surveyCells/,
    "iqAvailability/iqBackingAt must not read the coverage/survey plane");
  const clock = noComments("src/app/centre/capture-clock.ts");
  assert.match(readFileSync("src/app/centre/capture-clock.ts", "utf8"), /IQ_AVAILABILITY_REQUEST = "\/api\/recordings"/);
  assert.doesNotMatch(clock, /api\/coverage|api\/tiles/, "the poll that feeds state.iqAvailability stays off the coverage plane");
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

test("durationText keeps seconds: a 90 s retention is not rounded into minutes", () => {
  assert.equal(durationText(45), "45 s");
  assert.equal(durationText(89), "89 s");
  assert.equal(durationText(120), "2 min");
  assert.equal(durationText(3600), "1.0 h");
});

// ---- the rules, as drawn ----

test("ringRuleQuads: both rules land at their capture instants through the pane's own mapping", () => {
  const box = boxOf(880, 1000);
  const rules = ringRules(winOf(1000, 100, { t0S: 950, t1S: 1000 }), 1000)!;
  const quads = ringRuleQuads(rules, box, RECT);
  const ret = quads.filter((q) => q.id === "retention"), iq = quads.filter((q) => q.id === "iq-horizon");
  assert.ok(ret.length > 1, "the retention bound is drawn, dashed");
  assert.equal(iq.length, 1, "the IQ horizon is drawn, solid");
  for (const q of ret) assert.ok(Math.abs(midY(q) - clipYOf(900, box)) < 1e-9, "retention bound at edge − retention");
  assert.ok(Math.abs(midY(iq[0]) - clipYOf(950, box)) < 1e-9, "IQ horizon at buffered.t0");
  assert.deepEqual([iq[0].clip[0], iq[0].clip[2]], [-1, 1], "the IQ horizon spans the whole pane");
  // Order: retention first, so the IQ horizon is drawn on top when the two coincide.
  assert.ok(quads.indexOf(iq[0]) > quads.indexOf(ret[ret.length - 1]));
});

test("ringRuleQuads: strokes only, distinct from the coverage grey and from each other", () => {
  const box = boxOf(880, 1000);
  const quads = ringRuleQuads(ringRules(winOf(1000, 100, { t0S: 950, t1S: 1000 }), 1000), box, RECT);
  for (const q of quads) {
    assert.equal(q.kind, "time-rule");
    assert.ok(quadSizePx(q, RECT).hPx <= 4 + 1e-9, "a rule, never a wash over the cells it marks");
  }
  const grey = (c: readonly number[]) => Math.abs(c[0] - c[1]) < 0.05 && Math.abs(c[1] - c[2]) < 0.05;
  assert.ok(!grey(IQ_RULE_INK) && !grey(RETENTION_RULE_INK), "a neutral ink could be read as unobserved");
  assert.notDeepEqual(IQ_RULE_INK, RETENTION_RULE_INK);
  assert.equal(IQ_RULE_INK[3], 1, "opaque, so the ink on screen is exactly this value");
  assert.equal(RETENTION_RULE_INK[3], 1);
});

test("ringRuleQuads: an instant outside the pane draws nothing, and an unanswered window draws nothing", () => {
  // Pinning a rule to the pane's edge would claim a boundary at a time it is not.
  const rules = ringRules(winOf(1000, 100, { t0S: 950, t1S: 1000 }), 1000);
  assert.deepEqual(ringRuleQuads(rules, boxOf(960, 1000), RECT), [], "both instants are older than this pane");
  assert.deepEqual(ringRuleQuads(null, boxOf(880, 1000), RECT), []);
  assert.deepEqual(timeRuleQuads(Number.NaN, IQ_RULE_INK, "x", boxOf(880, 1000), RECT), []);
});

test("timeRuleQuads: dashes are laid out in device px and stay inside the pane", () => {
  const q = timeRuleQuads(950 * S, RETENTION_RULE_INK, "r", boxOf(880, 1000), RECT, { thickPx: 4, dashPx: 10, gapPx: 6 });
  assert.equal(q.length, Math.ceil(800 / 16));
  for (const d of q) {
    assert.ok(d.clip[0] >= -1 && d.clip[2] <= 1 && d.clip[2] > d.clip[0]);
    assert.ok(quadSizePx(d, RECT).wPx <= 10 + 1e-9);
  }
});

// ---- the extent, and the request ----

test("T-338 on the canvas: the retained window's extent is the ring's, never the history horizon", () => {
  // `timeExtent` is now live code: `surface.ts` floors the canvas's time extent at its `lo`, so the
  // retention window is always reachable however young the spectrum history is. The history horizon
  // may extend the extent further back (docs/16 §8: the canvas is also the history view); it can
  // never shorten it below the retention.
  const w = captureWindow({ window: { t0_s: 880, t1_s: 1000, span_s: 120, buffered: { t0_s: 990, t1_s: 1000 } } });
  assert.deepEqual(timeExtent(w), { lo: 880, hi: 1000 }, "the buffered span does not shrink it");
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /const ext = timeExtent\(w\);\s*if \(ext && preview\) preview\.extendTimeFloor\(ext\.lo \* S_TO_NS\)/,
    "the canvas's time floor is taken from the ring's window");
  const prev = readFileSync("src/surface/preview.ts", "utf8");
  assert.match(prev, /if \(!Number\.isFinite\(t0Ns\) \|\| !\(t0Ns < b\.t0Ns\)\) return false;/,
    "the floor only ever moves older, so a poll can never yank a pane");
});

test("the capture clock asks for the window alone, on a cadence, and is the only writer of captureWindow", () => {
  // Assert the request the client builds, not only the response it renders (T-367): no band means
  // the server folds no grid, and there is no t0/t1 because the window is the server's to state.
  assert.equal(CAPTURE_CLOCK_REQUEST, "/api/timeline?columns=1&rows=1");
  assert.doesNotMatch(CAPTURE_CLOCK_REQUEST, /t0|t1|f_lo|f_hi/);
  assert.ok(CAPTURE_CLOCK_MS > 0 && CAPTURE_CLOCK_MS <= 10_000, "fresh enough to catch the ring filling");
  // T-379: every other live-edge reader falls back to this state. Deleting the panel deleted its
  // only writer; this is the guard that the re-homed one exists and is started with the canvas.
  const writers = srcFiles().filter((f) => /setCaptureWindow\(/.test(readFileSync(f, "utf8")) && !f.endsWith("capture-slice.ts"));
  assert.deepEqual(writers, ["src/app/centre/capture-clock.ts"]);
  assert.match(readFileSync("src/app/centre/surface.ts", "utf8"), /^\s*startCaptureClock\(ctx\);/m);
});

test("Record IQ survived the panel: it records the viewport's band through the outputs route", () => {
  const src = readFileSync("src/app/centre/capture-clock.ts", "utf8");
  assert.match(src, /"\/api\/outputs\/record\/start", \{ band: \{ f_lo: span\.loHz, f_hi: span\.hiHz \}, kinds: \["iq"\] \}/);
  assert.match(src, /"\/api\/outputs\/record\/stop"/);
  // T-1004: mounted with the pane reporter — the button is per-pane chrome, and what it records
  // depends on whether the active pane is frozen.
  assert.match(readFileSync("src/app/centre/surface.ts", "utf8"), /recordIqButton\(ctx, \(\) => \{/, "it is mounted (in the viewport menu since T-882)");
});

test("T-1004: Record IQ says what it will record when the active viewport is FROZEN", () => {
  const span = { loHz: 99.6e6, hiHz: 102e6 };
  // Live (or a host with no panes): unchanged, and no mention of a past it is not recording.
  assert.equal(recordLabel(null), "Record IQ");
  assert.equal(recordLabel({ frozen: false, pane: "pane 2 of 2" }), "Record IQ");
  assert.match(recordTitle({ frozen: false, pane: "pane 2 of 2" }, span), /^Record raw IQ forward from now over 99\.600–102\.000 MHz/);
  assert.doesNotMatch(recordTitle(null, span), /frozen/);

  // Frozen: the word on the button says LIVE, and the sentence says both what it records and what
  // it does not — with the reason, so the refusal is a fact about IQ, not a UI opinion.
  const at = { frozen: true, pane: "pane 1 of 2" };
  assert.equal(recordLabel(at), "Record IQ (live)");
  const title = recordTitle(at, span);
  assert.match(title, /99\.600–102\.000 MHz/, "it still names the band that will be recorded");
  assert.match(title, /pane 1 of 2 is frozen behind the live edge/);
  assert.match(title, /not the past window on screen/);
  // And the same statement is made again when the recording actually starts.
  const note = frozenRecordNote(at, span);
  assert.match(note, /99\.600–102\.000 MHz/);
  assert.match(note, /not the frozen window pane 1 of 2 is showing/);
  assert.match(note, /cannot be recorded from the past/);
  // With one pane there is no pane name to use, and the sentence still stands on its own.
  assert.match(recordTitle({ frozen: true, pane: null }, null), /This viewport is frozen behind the live edge/);
  assert.match(recordTitle({ frozen: true, pane: null }, null), /the tuned span/);

  // The press path re-reads the pane at the click (a viewport freezes and follows as the user
  // scrubs), and the button re-states itself on the frame rather than when its menu was built.
  const src = readFileSync("src/app/centre/capture-clock.ts", "utf8");
  assert.match(src, /const where = at\(\);\s+client\.post/, "the press reads the pane state at the press");
  assert.match(src, /if \(where\?\.frozen\) store\.set\(toast\(frozenRecordNote\(where, span\)\)\)/);
  assert.match(readFileSync("src/app/centre/surface.ts", "utf8"), /recordBtn\.sync\(\);/, "the word is re-stated on the frame");
});

test("no default span: nothing on the capture window's path can fall back to a constant window", () => {
  // The regression T-338 removed: `WINDOW_S = 48 * 3600` sized the scrubber and offered times the
  // IQ ring had already overwritten.
  const src = readFileSync("src/app/centre/capture-window.ts", "utf8").replace(/\/\*[\s\S]*?\*\/|\/\/.*/g, "");
  assert.doesNotMatch(src, /(windowS|spanS)\s*:\s*number\s*=/, "a defaulted span is a constant in disguise");
  assert.doesNotMatch(src, /WINDOW_S|RETENTION_S\s*=/, "the constant itself is gone, not merely unused");
  assert.doesNotMatch(src, /\d+\s*\*\s*3600/);
});

test("T-386 CLOCK GUARD: no clock of the browser's own reaches the capture-window modules", () => {
  for (const f of ["src/app/centre/capture-window.ts", "src/app/centre/capture-clock.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["Date.now", "performance.now", "new Date", "toLocaleTimeString", "getTimezoneOffset"]) {
      assert.ok(!src.includes(word), `${f} must not contain "${word}"`);
    }
  }
});

test("the Capture panel is gone: no slot, no module, no pref, no grid row", () => {
  const html = readFileSync("src/app/index.html", "utf8");
  assert.doesNotMatch(html, /data-slot="capture"/);
  for (const f of srcFiles()) assert.ok(!f.startsWith("src/app/capture/"), `${f} survived the removal`);
  const css = readFileSync("src/app/base.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.doesNotMatch(css, /cap-collapsed|\.capture\s*\{|92px/);
  assert.match(css, /\.centre\s*\{[^}]*grid-template-rows:\s*minmax\(0,1fr\);/, "the surface takes the whole column");
  for (const f of ["src/app/shell-slice.ts", "src/app/shell.ts"]) {
    assert.doesNotMatch(readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/.*/g, ""), /captureCollapsed/);
  }
});

function srcFiles(dir = "src"): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir)) {
    const p = join(dir, e);
    if (statSync(p).isDirectory()) out.push(...srcFiles(p));
    else if (/\.(ts|html|css)$/.test(e)) out.push(p);
  }
  return out.sort();
}
