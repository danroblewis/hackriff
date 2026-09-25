// **The one place a cell's mark is decided** (T-440 + T-441, docs/16 §4/§5.5/§8.3, T-437 finding F3).
//
// F3, measured on real WebGL2: with the client tile budget below the working set, 101–198 tiles per
// frame rendered **grey** — and grey is this surface's load-bearing claim that the radio never
// looked there. A memory budget must never be able to manufacture that claim. So the rule is
// structural rather than careful:
//
//   1. **Grey is emitted only from a cell whose STATE BYTE says `unobserved`** — a byte that comes
//      from `coverage`, i.e. from the server. There is no other expression in the shader that can
//      produce it (asserted in ui/test/surface-honesty.test.ts by counting the constant's
//      occurrences in the generated source).
//   2. **"Not resident" is not a cell state at all.** It is a property of a *tile*, carried by a
//      different type ([[DrawKind]]), drawn by a different branch, and it can never reach this
//      table. A missing tile draws a resident coarser ancestor (upscaled, and *said*) or the
//      pending mark — never grey, never the bottom of the ramp.
//
// The tables below are the single source, and **every mark the renderer can draw is generated from
// them** — the T-397 pattern. T-397's defect was two implementations of one ramp drifting apart
// (the strips stopped at cyan); a second implementation of the grey rule would be the same defect
// on the claim that matters most. T-441 extends the generation to the honesty tiers and to the
// stand-in mark, so there is no hand-written colour arithmetic left in the fragment shader at all.
//
// **Presentation only.** These are marks for states the backend decided; nothing here reads a level,
// a unit or a scale. `state` comes from `/api/tiles`'s `coverage` plane, `value` from `grid.max_db`,
// `tier` from `resolution.source`, and `lo`/`hi` from the display range the caller already holds.

import { cmap } from "../cmap";

/** A cell's state, as the state plane stores it. The byte values are the wire order of nothing —
 * they are this module's own encoding, and the decoder is the only writer. */
export const CELL = {
  /** `coverage` state `"unobserved"`: nothing ever sampled this cell. **This is the only grey.** */
  UNOBSERVED: 0,
  /** `coverage` state `"observed"` and the pyramid holds a level: the value byte is a measurement. */
  OBSERVED: 1,
  /** Observed, `grid.max_db` is `null`, and the pyramid *did* fold frames here — so a level was
   * held and is not in hand now. `/api/coverage`'s own words for `shade: null`: *sampled, level not
   * retained*. Drawn differently from grey, and never as the bottom of the ramp. Also the fallback
   * when the tile carries no per-cell frame counts, because it is the mark that claims least. */
  NO_LEVEL: 2,
  /** `coverage` state `"unknown"` (T-423): the record that would say whether we looked is gone.
   * The fourth state is **not** grey and not a level; T-413 named the drawing, and it is the hatch
   * below. */
  UNKNOWN: 3,
  /**
   * **The fifth state (T-441): observed, and nothing has been folded here yet.**
   *
   * `coverage` says `observed` — the radio was demonstrably tuned here, at `duty` up to 1.0 — the
   * measurement is `null`, and `grid.frames` for the cell is **0**. T-446 measured it post-fix:
   * 216 measurement cells against 224 coverage cells at 433.92 MHz, the eight newest legitimately
   * looked-at-but-not-yet-written. **On a live edge this is the normal state of the newest cells.**
   *
   * It is not [[UNKNOWN]] (that is a record that aged out, one horizon away) and it is not
   * [[NO_LEVEL]] (that is frames folded and no level surviving). The evidence that separates it is
   * on the wire and needs no clock and no guess: *zero frames folded into a cell we know we
   * sampled*.
   *
   * **Its mark must not promise arrival.** T-446's defect produced exactly this cell — coverage
   * observed, `frames_late` climbing, nothing ever written — for an hour of capture time. A mark
   * reading "loading, back shortly" would have made that bug invisible; a mark reading "we looked
   * and hold nothing" makes a whole band of it plainly visible against its neighbours. So the mark
   * states the absence and stops there.
   */
  AWAITING: 4,
  /**
   * **The sixth state (T-520, ADR-0020): unobserved HERE, but a real measurement of an earlier
   * time is carried down to this cell** — the band was swept, then the radio left.
   *
   * `coverage` says `unobserved` for this cell, and `/api/tiles`'s `shadow` plane carries the
   * band's newest max-hold from `last_t_s` and earlier. That is a measurement, just **not of this
   * cell**: the last-known / stale honesty tier. It is drawn from the *same* ramp at the *same*
   * scale (the colour of a shadow is the colour that level had when it was live), then pushed down
   * under a hard brightness ceiling and ruled with a texture no live cell ever carries — see
   * [[SHADOW_MARK]] for why both, and why not graded by age.
   *
   * **Grey is untouched.** A cell is SHADOW only on positive evidence (a run over it); an
   * `unobserved` cell with no run is still [[UNOBSERVED]], THE grey, and nothing else is.
   */
  SHADOW: 5,
  /**
   * **The seventh state (T-595): sampled, and deliberately excluded from analysis** — the
   * receiver's own DC/LO-leakage notch at the tuned centre.
   *
   * `coverage` says `"excluded"`. The radio was here, the pyramid holds rows here, and the only
   * thing that did not happen is the detector's analysis. So this is **not** grey and not an
   * absence: the measurement is drawn on the ordinary ramp at the ordinary scale, with an ink ruled
   * along the *frequency* axis over it — the axis the exclusion is a stripe on, and the only mark
   * on this surface ruled that way ([[CELL.SHADOW]] rules along time).
   *
   * It is the fourth thing that used to wear [[CELL.UNOBSERVED]]'s mark: past the IQ ring's horizon
   * the observation log's notch punched a 25 kHz hole in coverage, and T-588 measured 1 212 cells
   * holding a measurement and reading `unobserved` — every one of them this notch.
   *
   * **It does not set the display range and is not plotted on the trace.** Inside the notch the
   * level is the receiver's own LO leakage, not a measurement of the air; drawing it is honest
   * (we have it, and we say why it is marked), letting it decide the ramp or read as a signal
   * would not be. `vscale.ts` and `trace.ts` both take `CELL.OBSERVED` and nothing else.
   */
  EXCLUDED: 6,
} as const;

export type CellState = (typeof CELL)[keyof typeof CELL];

/**
 * The patterns, as **GLSL boolean expressions over `vec2 px` (device pixels within the quad) and
 * `vec2 p` (the pattern pitch, per axis)**.
 *
 * They are strings rather than code because they must be *one* implementation: the fragment shader
 * is generated from them, and [[patternHit]] evaluates the same string for the CPU-side reference
 * rasteriser the tests draw frames with. A pattern written twice is T-397 again.
 *
 * The geometry is chosen so that no two marks that can appear on one screen share a shape:
 * the stand-in hatch runs **down-right**, the `unknown` hatch runs **up-right** at a different
 * pitch, `awaiting` and the `spectrum-history` tier are **dots** at different pitches over grounds
 * that can never be confused (a flat cold ground with no level, versus a ramp colour), and the
 * `survey-overview` tier is an **axis-aligned lattice** — the only mark that is neither diagonal
 * nor dotted, and the only one whose pitch is a measurement rather than a constant.
 */
export const PATTERNS = {
  /** Diagonal, down-right. The stand-in (upscaled-ancestor) mark. */
  hatchDown: "fract((px.x + px.y) / p.x) < 0.35",
  /** Diagonal, up-right. `unknown`. */
  hatchUp: "fract((px.y - px.x) / p.x) < 0.5",
  /** Square dots on a lattice. `awaiting`, and the `spectrum-history` tier. */
  dots: "fract(px.x / p.x) < 0.34 && fract(px.y / p.y) < 0.34",
  /** Axis-aligned rules. The `survey-overview` tier, drawn at the **true measured cell pitch**. */
  grid: "fract(px.x / p.x) < 0.10 || fract(px.y / p.y) < 0.10",
  /** **Both** diagonals: an X. The refused mark ([[REFUSED_MARK]], T-499) — the one mark that is a
   * statement about *this client's* last request rather than about the radio, and the only one on
   * the surface that is neither a single diagonal, nor dots, nor a lattice. */
  cross: "fract((px.x + px.y) / p.x) < 0.22 || fract((px.y - px.x) / p.x) < 0.22",
  /** Horizontal rules only: scanlines. The last-known [[CELL.SHADOW]] mark (T-520) — the one mark
   * ruled along a single axis, and ruled along *time*, the axis the value is carried down. */
  scan: "fract(px.y / p.y) < 0.2",
  /** Vertical rules only. The [[CELL.EXCLUDED]] mark (T-595) — ruled along *frequency*, because a
   * DC notch is a stripe on the frequency axis; the one mark that is the transpose of [[scan]]. */
  stripe: "fract(px.x / p.x) < 0.4",
} as const;

export type PatternName = keyof typeof PATTERNS;

/** A 2-vector, in the shader's terms. */
export interface Vec2 { readonly x: number; readonly y: number }

type PatternFn = (px: Vec2, p: Vec2, fract: (v: number) => number) => boolean;

const fract = (v: number) => v - Math.floor(v);

/**
 * The same predicates as [[PATTERNS]], in TypeScript.
 *
 * **Why these are written out rather than `new Function`-ed from the GLSL string** (T-450). The
 * original compiled the expression at module scope, which is the nicer shape — one implementation,
 * no possibility of drift — and it works everywhere except the one place this code has to run.
 * `hk serve` sends `Content-Security-Policy: default-src 'self'` with no `unsafe-eval`, so the
 * `new Function` call throws **while the module is being evaluated**, and since `surface.ts` imports
 * this file that took the *entire renderer* down in the browser. Nothing caught it before T-450
 * because the app never imported this module and node's test runner has no CSP: the first page to
 * mount the surface was also the first to find out.
 *
 * The anti-drift guarantee is kept, and made stronger, by moving the `new Function` into the test
 * instead of deleting it: `ui/test/surface-tiers.test.ts` compiles every string in [[PATTERNS]] and
 * asserts it agrees with the entry below over a dense grid of pixels and pitches. So a transcription
 * that drifts fails a test, rather than being prevented by a construct the product's own CSP
 * forbids — an *asserted* equivalence in place of an assumed one.
 */
const COMPILED: Readonly<Record<PatternName, PatternFn>> = {
  hatchDown: (px, p, fr) => fr((px.x + px.y) / p.x) < 0.35,
  hatchUp: (px, p, fr) => fr((px.y - px.x) / p.x) < 0.5,
  dots: (px, p, fr) => fr(px.x / p.x) < 0.34 && fr(px.y / p.y) < 0.34,
  grid: (px, p, fr) => fr(px.x / p.x) < 0.10 || fr(px.y / p.y) < 0.10,
  cross: (px, p, fr) => fr((px.x + px.y) / p.x) < 0.22 || fr((px.y - px.x) / p.x) < 0.22,
  scan: (px, p, fr) => fr(px.y / p.y) < 0.2,
  stripe: (px, p, fr) => fr(px.x / p.x) < 0.4,
};

/** Is this pixel on the pattern? The same expression the shader runs, evaluated on the CPU. */
export function patternHit(name: PatternName, px: Vec2, p: Vec2): boolean {
  return COMPILED[name](px, p, fract);
}

/** RGB, 0…1, linear — the same space [[cmap]] works in. */
export type Rgb = readonly [number, number, number];

/** How a cell of a given state is marked. `ramp` means "colour it from the measurement". */
export type CellMark =
  | { readonly kind: "ramp" }
  | { readonly kind: "flat"; readonly rgb: Rgb }
  /** A ground with a pattern inked over it — a mark that reads as *texture*, so it can never be
   * mistaken for a level however the display range moves. */
  | { readonly kind: "pattern"; readonly rgb: Rgb; readonly ink: Rgb; readonly pattern: PatternName; readonly pitchPx: number }
  /** **The ramp, remembered** (T-520): `cmap(x) * gain` — the same ramp at the same scale, held
   * under a hard ceiling — with a pattern inked over it in a fixed colour. The ground carries the
   * level; the ink says it is not a level of *this* cell. */
  | { readonly kind: "shadow"; readonly gain: number; readonly ink: Rgb; readonly pattern: PatternName; readonly pitchPx: number }
  /** **The ramp, qualified** (T-595): `cmap(x)` at full scale — the measurement, undimmed, because
   * it is a measurement of *this* cell — with a pattern inked over it saying the analysis did not
   * run here. Unlike [[CellMark]]'s `shadow` there is no gain: nothing about the level is in
   * doubt, only what was done with it. */
  | { readonly kind: "inked"; readonly ink: Rgb; readonly pattern: PatternName; readonly pitchPx: number };

/**
 * The rule, as data. Index is the state byte.
 *
 * The marks are deliberately far apart in hue, lightness **and shape**: a viewer must be able to
 * tell "never looked" from "looked, kept nothing" from "no longer know" from "looked, nothing
 * folded yet" at a glance, and T-413 declined the obvious rendering precisely because reusing one
 * mark for two of them spells *looked and it was quiet* as *never looked*. Seven states, seven
 * marks.
 */
export const CELL_MARKS: readonly CellMark[] = [
  { kind: "flat", rgb: [0.155, 0.16, 0.18] }, // UNOBSERVED — THE grey
  { kind: "ramp" }, //                           OBSERVED
  { kind: "flat", rgb: [0.1, 0.19, 0.17] }, //   NO_LEVEL — sampled, level not retained
  // UNKNOWN — the fourth state, drawn as T-423 said it would be: hatched (T-413).
  { kind: "pattern", rgb: [0.15, 0.1, 0.18], ink: [0.44, 0.3, 0.52], pattern: "hatchUp", pitchPx: 7 },
  // AWAITING — the fifth. Sparse dots on a cold ground: visibly *empty*, and visibly not grey.
  { kind: "pattern", rgb: [0.08, 0.1, 0.15], ink: [0.3, 0.42, 0.6], pattern: "dots", pitchPx: 9 },
  // SHADOW — the sixth (T-520). The last-known spectrum, dimmed under a ceiling and scanlined.
  // The gain is the SHIPPED DEFAULT and the only place it is written (T-523 raised it from 0.25;
  // `./shadow-gain.ts` re-exports this number rather than keeping a second copy of it). Why 0.32
  // and not higher is [[SHADOW_MARK]]'s second paragraph: above ~0.349 the always-brighter-than-
  // shadow band collapses off the bottom of the ramp.
  { kind: "shadow", gain: 0.32, ink: [0.24, 0.19, 0.1], pattern: "scan", pitchPx: 5 },
  // EXCLUDED — the seventh (T-595). The measurement on the ordinary ramp, ruled along frequency in
  // a cold ink on no position of the ramp: "we sampled this and did not analyse it".
  { kind: "inked", ink: [0.52, 0.56, 0.62], pattern: "stripe", pitchPx: 4 },
];

/**
 * **The last-known mark** (T-520), named so the tests can hold it to its two guarantees.
 *
 * A shadow is a real measurement drawn where the radio is *not* looking, so the only unacceptable
 * failure is a viewer reading it as live. Two independent defences, because either alone fails
 * somewhere on the ramp:
 *
 *  1. **A hard brightness ceiling.** The ground is `cmap(x) * gain`, so no shadow pixel is brighter
 *     than `gain` (0.32) in any channel or in luminance — the ramp's white, remembered, is a dark
 *     grey-white. Every live colour from about a third of the way up the ramp (the cyan on) is
 *     brighter than any shadow can be, so **a signal in shadow can never look like a signal on
 *     air.** The hue is kept on purpose: a remembered carrier is still recognisably *which* level
 *     it was, which is the point of showing it.
 *
 *     **Why the default is 0.32 and not more** (T-523: the user reported the shadow as too dark).
 *     The ramp's luminance is not monotonic — the red stop at x=0.9 is `lum ≈ 0.349`, a dip between
 *     the yellow below it and the white above. So "live clears the brightest possible shadow from
 *     here up" holds over the whole cyan→red span only while `gain < 0.349`: at 0.25 the band began
 *     at x≈0.273, at 0.32 it begins at x≈0.313 (unchanged, still about a third), and at 0.35 it
 *     jumps to x≈0.902 — brightness alone stops separating a remembered carrier from a live one
 *     across two thirds of the ramp. 0.32 is therefore the brightest default that keeps defence 1
 *     doing the work the paragraph above claims for it, and it is ~28 % brighter than 0.25 on
 *     screen. A viewer who wants more can have it — [[SHADOW_GAIN_MAX]] is 0.7 — but past the dip
 *     it is defence 2, the scanlines, carrying the distinction, which is a choice a viewer makes
 *     rather than one that ships. `surface-honesty.test.ts` asserts both ends: the ceiling at 0.7,
 *     and that the DEFAULT keeps its crossing point in the lower third.
 *  2. **A texture no live cell carries.** The ceiling cannot help at the bottom of the ramp: a
 *     remembered noise floor and a live noise floor are both near-black. So every shadow cell is
 *     ruled with horizontal scanlines in a fixed warm ink that is on no position of the ramp, not
 *     the grey, not magenta, and lighter than a dimmed noise floor — visible exactly where the
 *     ceiling is not. Horizontal because the value is carried down the *time* axis; single-axis
 *     because no other mark on this surface is (the survey lattice is both axes, and darkens).
 *
 * **Not graded by age, deliberately.** Darker-with-age converges every old shadow on the dark end
 * of the ramp, and from there on grey, [[PENDING]] and [[BACKDROP]] — eroding precisely the
 * distinction this state exists to draw. Age is carried on the wire (`last_t_s`), exact, and is a
 * number to *read* (hover), not a shade to guess from. A constant mark is also one a viewer learns
 * once.
 */
export const SHADOW_MARK = CELL_MARKS[CELL.SHADOW] as Extract<CellMark, { kind: "shadow" }>;

/** The grey, named once so a test can count its occurrences in the generated shader. */
export const GREY: Rgb = (CELL_MARKS[CELL.UNOBSERVED] as { rgb: Rgb }).rgb;

/** The honesty tiers, in the byte order the renderer passes them as `uTier`. */
export const TIERS = ["live-iq", "spectrum-history", "survey-overview"] as const;
export type TierName = (typeof TIERS)[number];
export const TIER: Readonly<Record<"LIVE_IQ" | "SPECTRUM_HISTORY" | "SURVEY_OVERVIEW", number>> = {
  LIVE_IQ: 0,
  SPECTRUM_HISTORY: 1,
  SURVEY_OVERVIEW: 2,
};

/** The byte for a tier name. An unrecognised tier is the **most qualified** one, never the most
 * trusted: nothing said is never permissive (the `bias_tee: "unknown"` rule). */
export function tierByte(name: string): number {
  const i = (TIERS as readonly string[]).indexOf(name);
  return i < 0 ? TIER.SURVEY_OVERVIEW : i;
}

/**
 * How a tier qualifies a **measurement**.
 *
 * Two properties this shape exists to keep:
 *
 *  - **The tier never moves the ramp.** A tier mark darkens only the pixels it inks; a cell's
 *    unmarked pixels are exactly `cmap(x)`. T-342's defect was the same energy reading as two
 *    strengths on one screen, and a per-tier wash would re-import it with a new cause.
 *  - **The tier qualifies nothing but a measurement.** It is applied only where the state byte says
 *    [[CELL.OBSERVED]], because a cell with no measurement has no resolution to overstate — and
 *    because a tier wash over grey would make a second grey, which is the one thing this module is
 *    for.
 *
 * `live-iq` is the unmarked tier. Marks are qualifications, so the tier with nothing to qualify
 * carries none, and every mark on screen is therefore something the renderer is *saying*.
 */
export type TierMark =
  | { readonly kind: "none" }
  | {
      readonly kind: "pattern";
      readonly pattern: PatternName;
      /** A constant device-pixel pitch, or `"source-cell"`: the on-screen size of one cell the
       * front end **actually measured**, which is what makes a replicated tile show its true
       * resolution instead of a smooth upscale (docs/16 §4, T-342's rule, T-411's failure). */
      readonly pitchPx: number | "source-cell";
      readonly darken: number;
    };

export const TIER_MARKS: readonly TierMark[] = [
  { kind: "none" }, // live-iq — the surface at full trust
  // spectrum-history: a fine stipple. "This is the record, not the live stream."
  { kind: "pattern", pattern: "dots", pitchPx: 5, darken: 0.18 },
  // survey-overview: the lattice of the cells that were really measured, drawn over the cells the
  // tile happens to serve. **Replication is allowed and declared, never passed off as detail.**
  { kind: "pattern", pattern: "grid", pitchPx: "source-cell", darken: 0.34 },
];

/**
 * The mark for a tile that is **not the tile asked for** — a resident coarser ancestor, upscaled
 * (docs/16 §5.5: "draws the coarser parent upscaled and says so — never grey"). A residency, never
 * a cell state, so it is a separate table and a separate branch.
 */
export const FALLBACK_MARK = {
  pattern: "hatchDown" as PatternName,
  pitchPx: 10,
  darken: 0.2,
  /** A slight wash toward grey-50 over the whole quad, so a stand-in reads as one at a glance and
   * not only where the hatch falls. Toward 0.5, never toward [[GREY]]. */
  wash: 0.1,
  washTo: [0.5, 0.5, 0.5] as Rgb,
} as const;

/**
 * **Not a cell state.** What a pane is drawing at a place, which is a fact about *tiles in memory*
 * and never about the radio. Keeping it a separate type is the mechanical half of F3's fix: a
 * residency can never be passed where a [[CellState]] is expected, so an eviction cannot be spelled
 * as an observation.
 */
export type DrawKind =
  /** A resident tile at the pane's own level: the cells are drawn through [[CELL_MARKS]]. */
  | "tile"
  /** A resident *coarser* tile standing in for a missing one, upscaled — and marked as a fallback
   * (docs/16 §5.5: "draws the coarser parent upscaled and says so — never grey"). */
  | "fallback"
  /** Nothing resident and no ancestor: the tile has not arrived. Drawn as [[PENDING]], which is a
   * mark again, and never grey. */
  | "pending"
  /** **Asked, and no usable answer came back** (T-499): the route refused the place permanently
   * (T-479), or the server is unreachable and this client is waiting out its backoff. Drawn as
   * [[REFUSED_MARK]]. */
  | "refused";

/** The mark for "we have not loaded this yet" — a memory/latency fact, visibly not the grey. */
export const PENDING: Rgb = [0.07, 0.075, 0.1];

/**
 * **The mark for a place this client asked about and got nothing usable for** (T-499).
 *
 * # Why this is not grey, and not a clamp
 *
 * Grey is this surface's one claim that *the radio never looked*, and it is emitted only from a
 * coverage state byte the server wrote (the rule at the top of this file). "I asked and the answer
 * was unusable" is a fact about **this client's last request**, not about the radio, so it cannot
 * be spelled in that vocabulary at all — it belongs in [[DrawKind]], beside [[PENDING]], which is
 * already the lane for "not loaded is not unobserved".
 *
 * It needs to be **its own** mark rather than [[PENDING]] because the two are different in the one
 * way a user acts on: `pending` is *wait*, `refused` is *nothing is coming until something changes*.
 * A dead stream painted as `pending` is a progress bar that never finishes — the same defect as
 * T-441's `AWAITING`, whose whole point is that a mark must not promise arrival it cannot make.
 *
 * The shape is an **X** ([[PATTERNS.cross]]), at a coarse pitch, over the pending ground: no other
 * mark on this surface is a cross, and the ink is a neutral slate — deliberately **not** magenta, so
 * it can never be confused with `unknown`'s hatch (whose ink is what T-499's user saw), and far from
 * anything on [[cmap]]'s ramp, so it can never read as a level.
 */
export const REFUSED_MARK = {
  rgb: [0.07, 0.075, 0.1] as Rgb,
  ink: [0.34, 0.36, 0.4] as Rgb,
  pattern: "cross" as PatternName,
  pitchPx: 14,
} as const;

/** The mark for a place no pane covers (between panes). Not a claim about anything. */
export const BACKDROP: Rgb = [0.04, 0.04, 0.05];

/**
 * **The coverage-fog layer** (T-807 / MAP-07, docs/24 §3b and §13.3): which states the layers menu's
 * `coverage` switch governs, and what they draw as while it is off.
 *
 * The fog is the surface's statement of **what is not known**: [[CELL.UNOBSERVED]] (THE grey —
 * nothing ever looked) and [[CELL.UNKNOWN]] (the record that would say is gone). Those two are
 * the fog. Every other state carries, or is a fact about, a measurement of *this* cell or band —
 * `observed`, `no level held`, `nothing folded yet`, the last-known [[CELL.SHADOW]] and the
 * [[CELL.EXCLUDED]] notch — and is drawn whether or not the fog is shown: hiding a measurement we
 * hold is "we have it but did not render it", and stripping the notch's ink would let the
 * receiver's own LO leakage read as a signal on air.
 *
 * **Hidden is not grey and not a level.** A fog cell with the layer off draws [[FOG_HIDDEN]] — a
 * flat, neutral bare ground (the mockup's "bare ground"), darker than [[GREY]] so the absence
 * recedes, with no texture (every measurement has one) and off the ramp. It is also never the
 * ramp's bottom: `cmap(0)` is blue-black, this is neutral. It is a *choice the viewer made*, stated
 * in words by the pane's readout, never a claim about the radio — so it cannot manufacture
 * "observed" out of "unobserved", and grey still comes from one state byte and nothing else.
 *
 * It is a flag on **this** rule, not a quad (§13.3: "the coverage switch sets a cell-rule flag; it
 * never adds a quad") — the generated `cellMark` takes it, so the shader and [[cellPixel]] cannot
 * disagree about which states it hides.
 */
export const FOG_STATES: readonly CellState[] = [CELL.UNOBSERVED, CELL.UNKNOWN];

/** The bare ground a fog state draws as while the coverage layer is hidden. Flat, neutral, darker
 * than [[GREY]], and on no position of the ramp. */
export const FOG_HIDDEN: Rgb = [0.085, 0.09, 0.095];

/** Is this state one the coverage-fog layer governs? */
export function isFogState(state: number): boolean {
  return (FOG_STATES as readonly number[]).includes(state);
}

/** The rule in TypeScript, for tests and for anything that must reason about a cell off-GPU. */
export function markFor(state: number): CellMark {
  return CELL_MARKS[state] ?? CELL_MARKS[CELL.UNOBSERVED];
}

const mix = (a: Rgb, b: Rgb, f: number): [number, number, number] =>
  [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f, a[2] + (b[2] - a[2]) * f];

/** What a `darken` leaves behind, rounded to four places **before** either consumer sees it, so the
 * literal compiled into the shader and the number the CPU rule multiplies by are the same number
 * and not two roundings of one intention. */
const keep = (darken: number) => Math.round((1 - darken) * 1e4) / 1e4;

/**
 * **The whole fragment rule, on the CPU.** One cell, one pixel, exactly what the generated shader
 * computes for the same inputs — so a test can rasterise a frame from the draws the renderer
 * actually submitted and assert on *pixels*, not on uniforms.
 *
 * `x` is the already-normalised 0…1 position on the ramp; `px` is the pixel's position inside the
 * quad in device pixels; `srcPx` is the on-screen size of one measured cell.
 */
export function cellPixel(args: {
  readonly state: number;
  readonly x: number;
  readonly px: Vec2;
  readonly tier: number;
  readonly srcPx: Vec2;
  readonly fallback: boolean;
  /** The shadow's brightness multiplier, client-adjustable (T-526, `./shadow-gain.ts`). Defaults to
   * [[SHADOW_MARK]]'s own gain, which is what every caller that does not pass one still gets. */
  readonly shadowGain?: number;
  /** The pane's coverage-fog layer (T-807). Defaults to shown — the registry's default. */
  readonly fog?: boolean;
}): [number, number, number] {
  const m = markFor(args.state);
  let col: [number, number, number];
  if (args.fog === false && isFogState(args.state)) col = [...FOG_HIDDEN] as [number, number, number];
  else if (m.kind === "ramp") col = cmap(args.x);
  else if (m.kind === "shadow") {
    const gain = args.shadowGain ?? m.gain;
    if (patternHit(m.pattern, args.px, { x: m.pitchPx, y: m.pitchPx })) col = [...m.ink] as [number, number, number];
    else { const c = cmap(args.x); col = [c[0] * gain, c[1] * gain, c[2] * gain]; }
  } else if (m.kind === "inked") {
    col = patternHit(m.pattern, args.px, { x: m.pitchPx, y: m.pitchPx })
      ? ([...m.ink] as [number, number, number])
      : cmap(args.x);
  } else if (m.kind === "flat") col = [...m.rgb] as [number, number, number];
  else col = [...(patternHit(m.pattern, args.px, { x: m.pitchPx, y: m.pitchPx }) ? m.ink : m.rgb)] as [number, number, number];

  if (args.state === CELL.OBSERVED) {
    const t = TIER_MARKS[args.tier] ?? TIER_MARKS[TIER.SURVEY_OVERVIEW];
    if (t.kind === "pattern") {
      const p = t.pitchPx === "source-cell" ? args.srcPx : { x: t.pitchPx, y: t.pitchPx };
      const k = keep(t.darken);
      if (patternHit(t.pattern, args.px, p)) col = [col[0] * k, col[1] * k, col[2] * k];
    }
  }
  if (args.fallback) {
    col = mix(col, FALLBACK_MARK.washTo, FALLBACK_MARK.wash);
    if (patternHit(FALLBACK_MARK.pattern, args.px, { x: FALLBACK_MARK.pitchPx, y: FALLBACK_MARK.pitchPx })) {
      const k = keep(FALLBACK_MARK.darken);
      col = [col[0] * k, col[1] * k, col[2] * k];
    }
  }
  return col;
}

const num = (v: number) => (Number.isInteger(v) ? v.toFixed(1) : String(v));
const vec3 = (c: Rgb) => `vec3(${c.map(num).join(",")})`;
const vec2 = (v: number) => `vec2(${num(v)},${num(v)})`;
const patFn = (name: PatternName) => `pat_${name}`;

/**
 * The same rules as GLSL, generated from [[PATTERNS]], [[CELL_MARKS]], [[TIER_MARKS]] and
 * [[FALLBACK_MARK]]:
 *
 * ```glsl
 * vec3 cellMark(int s, float x, vec2 px, float gain, bool fog); // the seven states; `gain` is the shadow's only;
 *                                                                //   `fog` false hides FOG_STATES (T-807)
 * vec3 tierMark(int t, vec3 col, vec2 px, vec2 srcPx);  // the three honesty tiers
 * vec3 fallbackMark(vec3 col, vec2 px);           // the stand-in
 * ```
 *
 * Requires `cmap` (ui/src/cmap.ts's [[CMAP_GLSL]], the one ramp) to be declared before it.
 *
 * Generated, not written out, so the grey in the shader and the grey in [[CELL_MARKS]] cannot
 * become two greys — and, since T-441, so the tier a frame draws and the tier a test reasons about
 * cannot become two tiers either.
 */
export const CELL_RULE_GLSL: string = (() => {
  const pats = (Object.keys(PATTERNS) as PatternName[]).map(
    (k) => `bool ${patFn(k)}(vec2 px, vec2 p) { return ${PATTERNS[k]}; }`,
  );

  // `gain` is a parameter, not a baked-in literal (T-526): the shadow's brightness multiplier is
  // user-adjustable client-side, so the generated function takes it from the caller (`uShadowGain`
  // in surface.ts) rather than compiling [[SHADOW_MARK]]'s default into the shader text. Every other
  // mark ignores the parameter; only the shadow branch reads it, exactly as `cellPixel` does above.
  const cell = CELL_MARKS.map((m, s) => {
    const body =
      m.kind === "ramp"
        ? "cmap(x)"
        : m.kind === "shadow"
          ? `(${patFn(m.pattern)}(px, ${vec2(m.pitchPx)}) ? ${vec3(m.ink)} : cmap(x) * gain)`
          : m.kind === "inked"
          ? `(${patFn(m.pattern)}(px, ${vec2(m.pitchPx)}) ? ${vec3(m.ink)} : cmap(x))`
          : m.kind === "flat"
          ? vec3(m.rgb)
          : `(${patFn(m.pattern)}(px, ${vec2(m.pitchPx)}) ? ${vec3(m.ink)} : ${vec3(m.rgb)})`;
    return s === CELL_MARKS.length - 1 ? `  return ${body};` : `  if (s == ${s}) return ${body};`;
  });

  const tier = TIER_MARKS.map((t, i) => {
    const body =
      t.kind === "none"
        ? "col"
        : `(${patFn(t.pattern)}(px, ${t.pitchPx === "source-cell" ? "srcPx" : vec2(t.pitchPx)}) ? col * ${num(keep(t.darken))} : col)`;
    return i === TIER_MARKS.length - 1 ? `  return ${body};` : `  if (t == ${i}) return ${body};`;
  });

  const f = FALLBACK_MARK;
  const r = REFUSED_MARK;
  return `
${pats.join("\n")}

vec3 cellMark(int s, float x, vec2 px, float gain, bool fog) {
  if (!fog && (${FOG_STATES.map((f) => `s == ${f}`).join(" || ")})) return ${vec3(FOG_HIDDEN)};
${cell.join("\n")}
}

vec3 tierMark(int t, vec3 col, vec2 px, vec2 srcPx) {
${tier.join("\n")}
}

vec3 fallbackMark(vec3 col, vec2 px) {
  vec3 c = mix(col, ${vec3(f.washTo)}, ${num(f.wash)});
  return ${patFn(f.pattern)}(px, ${vec2(f.pitchPx)}) ? c * ${num(keep(f.darken))} : c;
}

vec3 refusedMark(vec2 px) {
  return ${patFn(r.pattern)}(px, ${vec2(r.pitchPx)}) ? ${vec3(r.ink)} : ${vec3(r.rgb)};
}`;
})();

/** The refused mark on the CPU — the same expression `refusedMark` compiles to, for the tests and
 * for anything that must reason about a refused place off-GPU. */
export function refusedPixel(px: Vec2): [number, number, number] {
  const hit = patternHit(REFUSED_MARK.pattern, px, { x: REFUSED_MARK.pitchPx, y: REFUSED_MARK.pitchPx });
  return [...(hit ? REFUSED_MARK.ink : REFUSED_MARK.rgb)] as [number, number, number];
}
