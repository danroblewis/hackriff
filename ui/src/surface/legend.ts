// The legend (T-450): **what every mark on the surface means, painted by the rule that draws it**.
//
// A surface whose load-bearing claim is *"grey means nothing ever looked there"* is only honest if a
// viewer can tell the grey from the four other things that are also dark — and T-437 measured a
// first screen that is 99.4 % of exactly those marks. So the preview ships a key.
//
// **The swatches are not drawn twice.** Each one is rasterised through [[cellPixel]], the same
// function `ui/src/surface/cellrule.ts` generates the fragment shader from, with the same state
// byte, tier byte and fallback flag the renderer would pass. A hand-drawn legend would be a second
// implementation of the one rule this milestone most needs to have only one of (T-397's defect: two
// implementations of one ramp, drifting until the strips stopped at cyan). If a mark changes, the
// legend changes with it, because there is nothing here to keep in step.

import { CELL, PENDING, TIER, cellPixel, type Rgb, type Vec2 } from "./cellrule";

/** One row of the key: a name, what it claims, and how to paint a pixel of its swatch. */
export interface LegendEntry {
  readonly key: string;
  readonly label: string;
  /** What the mark *claims*, in the surface's own terms. Never "loading" for a coverage state. */
  readonly note: string;
  /** `px` is the pixel inside the swatch, `x` its 0…1 position along it (the ramp coordinate). */
  readonly pixel: (px: Vec2, x: number) => Rgb;
}

/** A measured-cell pitch for the `survey-overview` swatch. A drawing constant for the key only:
 * on the surface itself that pitch is `sourceCellPx`, i.e. a number the tile reported. */
const SWATCH_SRC_PX: Vec2 = { x: 14, y: 14 };

const cell = (state: number, tier = TIER.LIVE_IQ, fallback = false) =>
  (px: Vec2, x: number): Rgb => cellPixel({ state, x, px, tier, srcPx: SWATCH_SRC_PX, fallback });

/**
 * The key, in the order a viewer needs it: **the grey first**, because it is the claim, then the
 * four things that are not grey however dark they look, then the three honesty tiers, then the two
 * marks that are about this client's memory rather than about the radio.
 */
export function legendEntries(): readonly LegendEntry[] {
  return [
    {
      key: "unobserved",
      label: "Unobserved",
      note: "The grey, and the only grey. Nothing ever sampled this cell — not a loading state and not a quiet band.",
      pixel: cell(CELL.UNOBSERVED),
    },
    {
      key: "observed",
      label: "Observed",
      note: "A measurement, coloured from one display range shared by every viewport.",
      pixel: cell(CELL.OBSERVED),
    },
    {
      key: "no-level",
      label: "No level held",
      note: "Sampled, and frames were folded here, but the pyramid keeps no level for it now. Not grey and not the bottom of the ramp.",
      pixel: cell(CELL.NO_LEVEL),
    },
    {
      key: "unknown",
      label: "Past the record horizon",
      note: "The tune record that would say whether we looked has been discarded. Forgetting is not a measurement of nothing.",
      pixel: cell(CELL.UNKNOWN),
    },
    {
      key: "awaiting",
      label: "Nothing folded yet",
      note: "Sampled, zero frames folded into the cell. It states the absence and promises no arrival.",
      pixel: cell(CELL.AWAITING),
    },
    {
      key: "tier-history",
      label: "Tier · spectrum-history",
      note: "A measurement from the record rather than from live IQ. The stipple darkens; it never moves the ramp.",
      pixel: cell(CELL.OBSERVED, TIER.SPECTRUM_HISTORY),
    },
    {
      key: "tier-survey",
      label: "Tier · survey-overview",
      note: "Stitched from separate dwells, and drawn at the pitch of the cells the front end really measured — so replication is declared, not smoothed over.",
      pixel: cell(CELL.OBSERVED, TIER.SURVEY_OVERVIEW),
    },
    {
      key: "fallback",
      label: "Coarse stand-in",
      note: "A resident coarser tile upscaled while the finer one is missing — said, never passed off as the level it stands in for.",
      pixel: cell(CELL.OBSERVED, TIER.SPECTRUM_HISTORY, true),
    },
    {
      key: "pending",
      label: "Not loaded",
      note: "This client has not got the tile yet. A memory-and-latency fact about the browser, never a claim about the radio.",
      pixel: () => PENDING,
    },
  ];
}

/** Rasterise one swatch into an `ImageData`-shaped RGBA buffer, gamma-free like the renderer. */
export function swatchPixels(entry: LegendEntry, w: number, h: number): Uint8ClampedArray {
  const out = new Uint8ClampedArray(w * h * 4);
  for (let y = 0; y < h; y++) {
    for (let x = 0; x < w; x++) {
      // `px` is measured from the quad's own origin, exactly as the shader's `vQ * uSizePx` is.
      const rgb = entry.pixel({ x, y: h - 1 - y }, w > 1 ? x / (w - 1) : 0.5);
      const i = (y * w + x) * 4;
      out[i] = Math.round(rgb[0] * 255);
      out[i + 1] = Math.round(rgb[1] * 255);
      out[i + 2] = Math.round(rgb[2] * 255);
      out[i + 3] = 255;
    }
  }
  return out;
}
