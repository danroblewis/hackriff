// T-813 (MAP-13): the figure-ground / colour-blind-safety pass, as a regression guard rather than a
// one-off review. docs/23 §7: "state is never encoded in hue alone... the palette avoids red-green
// pairings." Two things are worth pinning down so a later palette edit cannot regress them silently:
//
//  - every mark ink that means something different from another still reads apart once a protanope
//    or deuteranope has looked at it ([[worstCaseDistance]] from `surface/cvd.ts`);
//  - any two feature classes whose box colour is the SAME (T-910's `unexplained` reuses its parent
//    state's colour) are told apart by shape/pattern instead — `SYMBOLOGY`'s outline/fill, not hue.

import { test } from "node:test";
import assert from "node:assert/strict";
import { colourDistance, cvdDistance, simulateCvd, worstCaseDistance } from "../src/surface/cvd";
import {
  ACTIVE_MARK, ARTIFACT_MARK, CANDIDATE_MARK, CONFIRMED_MARK, MEASUREMENT_MARK, OPEN_EDGE_MARK, PENDING_MARK,
  SELECTION_MARK, SYMBOLOGY, type FeatureClass,
} from "../src/surface/marks";
import { markKeyEntries } from "../src/surface/legend";
import { GREY } from "../src/surface/cellrule";

const rgb = (c: readonly [number, number, number, number]): readonly [number, number, number] => [c[0], c[1], c[2]];

test("MAP-13: simulateCvd is the identity on a fully achromatic colour", () => {
  // R == G == B has nothing for a confusion-line projection to move: every matrix row's weights
  // sum to 1, so a colour with no channel disagreement at all reproduces exactly, on both
  // deficiencies — the one case worth pinning as a sanity check on the matrices themselves.
  const grey: readonly [number, number, number] = [0.42, 0.42, 0.42];
  assert.deepEqual(simulateCvd(grey, "protanopia").map((v) => +v.toFixed(6)), [0.42, 0.42, 0.42]);
  assert.deepEqual(simulateCvd(grey, "deuteranopia").map((v) => +v.toFixed(6)), [0.42, 0.42, 0.42]);
});

test("MAP-13: colourDistance is zero for identical colours and positive for distinct ones", () => {
  assert.equal(colourDistance([0.3, 0.4, 0.5], [0.3, 0.4, 0.5]), 0);
  assert.ok(colourDistance(rgb(CONFIRMED_MARK), rgb(SELECTION_MARK)) > 0);
});

// Every ink the surface uses for a DIFFERENT claim (docs/23 §7's vocabulary): Confirmed/Candidate
// detections, an explained Artifact, a user Selection, a saved Measurement, an open interval's live
// edge, the in-progress pending region, and the grey (unobserved coverage). Two of these sharing a
// meaning would be a real collision; the rest must stay tellable apart under CVD simulation because
// nothing else distinguishes a selection from a measurement box on the surface.
const PALETTE: Record<string, readonly [number, number, number]> = {
  confirmed: rgb(CONFIRMED_MARK), candidate: rgb(CANDIDATE_MARK), artifact: rgb(ARTIFACT_MARK),
  selection: rgb(SELECTION_MARK), measurement: rgb(MEASUREMENT_MARK), openEdge: rgb(OPEN_EDGE_MARK),
  pending: rgb(PENDING_MARK), grey: GREY,
  // T-994: a box with an open output (Listen / decode / record / stream) — its own claim, "something
  // is being done with this", so it must read apart from every other ink.
  active: rgb(ACTIVE_MARK),
};

// Redmean units on 0..255 channels; ~20 is the threshold commonly used for "just noticeable" — this
// is deliberately looser (a coarse regression floor, not a colorimetric spec), but it is enough to
// catch two marks drifting onto the same ink.
const MIN_DISTANCE = 18;

test("MAP-13: every pair of differently-meaning marks stays apart under ordinary AND simulated CVD vision", () => {
  const names = Object.keys(PALETTE);
  const failures: string[] = [];
  for (let i = 0; i < names.length; i++) {
    for (let j = i + 1; j < names.length; j++) {
      const [a, b] = [names[i], names[j]];
      const d = worstCaseDistance(PALETTE[a], PALETTE[b]);
      if (d < MIN_DISTANCE) failures.push(`${a} vs ${b}: worst-case distance ${d.toFixed(1)} < ${MIN_DISTANCE}`);
    }
  }
  assert.deepEqual(failures, []);
});

test("MAP-13: protanopia/deuteranopia simulation moves a red-leaning hue further than a blue one", () => {
  // A sanity check on the simulation itself, not the palette: red and green project onto nearly the
  // same confusion line under either deficiency, so their simulated colours should draw close
  // together even though ordinary vision keeps them far apart. Blue is outside the red-green axis and
  // is comparatively untouched. This is what makes "no red-green-only encoding" the right rule to
  // enforce, rather than an arbitrary aesthetic preference.
  const red: readonly [number, number, number] = [0.85, 0.1, 0.1];
  const green: readonly [number, number, number] = [0.1, 0.85, 0.1];
  const ordinary = colourDistance(red, green);
  const proto = cvdDistance(red, green, "protanopia");
  const deutero = cvdDistance(red, green, "deuteranopia");
  assert.ok(proto < ordinary * 0.6, `protanopia collapsed red/green too little: ${proto} vs ${ordinary}`);
  assert.ok(deutero < ordinary * 0.6, `deuteranopia collapsed red/green too little: ${deutero} vs ${ordinary}`);
});

test("MAP-13: a class that can SHARE a box colour with another is told apart by outline or fill, not hue", () => {
  // `unexplained` deliberately reuses its parent state's ink (marks.ts's `signalMarkBoxes` /
  // legend.ts's `CLASS_RGB`: an unexplained Confirmed is still drawn teal, an unexplained Candidate
  // still lavender), so the guard that matters is structural: SYMBOLOGY's outline/fill, the cue
  // T-910 draws and `markKeyEntries` states in words, must differ from the state it could be
  // confused with by colour alone — confirmed and candidate are never drawn under `unexplained`'s
  // colour with its own symbology, because they are the two colours `unexplained` borrows.
  const sig = (c: FeatureClass) => `${SYMBOLOGY[c].outline}/${SYMBOLOGY[c].fill}`;
  assert.notEqual(sig("confirmed"), sig("unexplained"));
  assert.notEqual(sig("candidate"), sig("unexplained"));
  // Artifact is its own colour (grey, not a state colour), so sharing candidate/unexplained's
  // solid/no-fill signature is not a hue collision — but it must not become one silently either:
  // pin the fact that artifact's ink is distinct from every state colour under CVD simulation.
  assert.notEqual(sig("artifact"), sig("confirmed"));
});

test("MAP-13: the detections layer's key names every class the layer draws, in the box's own ink", () => {
  const keys = markKeyEntries();
  assert.deepEqual(keys.map((e) => e.key), ["confirmed", "candidate", "unexplained", "artifact", "curated"]);
  // Every note states the non-hue cue in words — the point of the key, for a viewer who cannot
  // rely on the swatch's colour alone.
  for (const e of keys) assert.match(e.note, /outline/);
  assert.match(keys.find((e) => e.key === "unexplained")!.note, /\?/);
});
