// **Colour-vision-deficiency safety, as a verification, not a runtime dependency** (T-813 / MAP-13).
//
// docs/23 §7: "State is never encoded in hue alone... roughly 8% of men have red-green colour-vision
// deficiency, so candidate/confirmed/unknown and observed/unobserved each carry a shape or pattern in
// addition to colour, and the palette avoids red-green pairings." The coverage-fog ramp already meets
// this by construction — every non-`OBSERVED` cell state is a flat near-grey or dark tone plus a
// pattern (`cellrule.ts`'s hatch/dots/scanlines), never a saturated hue a viewer must tell apart from
// another saturated hue. The part worth *checking* is the mark palette (`marks.ts`): five named inks
// that each mean something different (Confirmed, Candidate, an explained Artifact, a Selection, a
// saved Measurement), where a CVD viewer's only remaining cues, if two collapsed, would be shape/
// pattern (T-910's `SYMBOLOGY`) or context (nothing else is drawn there).
//
// This file is the check, not a rendering path — nothing in `ui/src` imports it outside tests. It
// simulates the two red-green deficiencies (protanopia, deuteranopia) with the standard linear-RGB
// approximation ([[Viénot, Brettel & Mollon 1999]](http://vision.psychol.cam.ac.uk/jdmollon/papers/colourmaps.pdf),
// the same matrices most browser CVD emulators use) and measures a simple perceptual distance between
// two colours *after* simulation, so a palette pass can assert "these two inks are still far enough
// apart once a protanope or deuteranope has looked at them", never "they differ in hue" (hue is
// exactly the cue that can vanish).

import type { Rgb } from "./cellrule";

export type CvdKind = "protanopia" | "deuteranopia";

/** Viénot/Brettel/Mollon's linear-RGB confusion-line projection, applied directly to the surface's
 * already-linear-ish [0,1] channel values — a relative-distance tool, not a colorimetric instrument. */
const MATRIX: Record<CvdKind, readonly [number, number, number, number, number, number, number, number, number]> = {
  protanopia: [0.567, 0.433, 0, 0.558, 0.442, 0, 0, 0.242, 0.758],
  deuteranopia: [0.625, 0.375, 0, 0.7, 0.3, 0, 0, 0.3, 0.7],
};

/** `rgb` as a CVD viewer of `kind` would see it. */
export function simulateCvd(rgb: Rgb, kind: CvdKind): Rgb {
  const [a, b, c, d, e, f, g, h, i] = MATRIX[kind];
  const [r, gr, bl] = rgb;
  return [a * r + b * gr + c * bl, d * r + e * gr + f * bl, g * r + h * gr + i * bl];
}

/** "Redmean" — the low-cost weighted-Euclidean approximation to perceptual colour distance
 * ([compuphase.com](https://www.compuphase.com/cmetric.htm)), on 0..255 channels. Cheap, order-
 * preserving and good enough to say "still tell these two apart", which is all a regression guard
 * needs — this is not a colorimetric certification. */
export function colourDistance(a: Rgb, b: Rgb): number {
  const [r1, g1, b1] = a.map((v) => v * 255);
  const [r2, g2, b2] = b.map((v) => v * 255);
  const rMean = (r1 + r2) / 2;
  const dr = r1 - r2, dg = g1 - g2, db = b1 - b2;
  return Math.sqrt((2 + rMean / 256) * dr * dr + 4 * dg * dg + (2 + (255 - rMean) / 256) * db * db);
}

/** `a` vs `b` as a `kind` CVD viewer would see them, by the same [[colourDistance]] metric. */
export function cvdDistance(a: Rgb, b: Rgb, kind: CvdKind): number {
  return colourDistance(simulateCvd(a, kind), simulateCvd(b, kind));
}

/**
 * The worst (smallest) distance between `a` and `b` across ordinary vision and both simulated
 * deficiencies — the number a "still distinguishable" assertion should use, because a palette that
 * is fine in one but collapses in another is not CVD-safe.
 */
export function worstCaseDistance(a: Rgb, b: Rgb): number {
  return Math.min(colourDistance(a, b), cvdDistance(a, b, "protanopia"), cvdDistance(a, b, "deuteranopia"));
}
