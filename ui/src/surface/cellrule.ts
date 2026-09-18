// **The one place grey is decided** (T-440, docs/16 §4/§5.5, T-437 finding F3).
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
// The table below is the single source, and the GLSL is **generated from it** — the T-397 pattern.
// T-397's defect was two implementations of one ramp drifting apart (the strips stopped at cyan);
// a second implementation of the grey rule would be the same defect on the claim that matters most.
//
// **Presentation only.** These are marks for states the backend decided; nothing here reads a level,
// a unit or a scale. `state` comes from `/api/tiles`'s `coverage` plane, `value` from `grid.max_db`,
// and `lo`/`hi` from the display range the caller already holds.

/** A cell's state, as the state plane stores it. The byte values are the wire order of nothing —
 * they are this module's own encoding, and the decoder is the only writer. */
export const CELL = {
  /** `coverage` state `"unobserved"`: nothing ever sampled this cell. **This is the only grey.** */
  UNOBSERVED: 0,
  /** `coverage` state `"observed"` and the pyramid holds a level: the value byte is a measurement. */
  OBSERVED: 1,
  /** Observed, but `grid.max_db` is `null` — sampled, level not retained. Drawn differently from
   * grey, and never as the bottom of the ramp (`/api/coverage`'s own rule for a null shade). */
  NO_LEVEL: 2,
  /** `coverage` state `"unknown"` (T-423): the record that would say whether we looked is gone.
   * The fourth state is **not** grey and not a level; T-441 owns its final drawing (T-413's hatch). */
  UNKNOWN: 3,
} as const;

export type CellState = (typeof CELL)[keyof typeof CELL];

/** How a cell of a given state is marked. `ramp` means "colour it from the measurement". */
export type CellMark =
  | { readonly kind: "ramp" }
  | { readonly kind: "flat"; readonly rgb: readonly [number, number, number] };

/**
 * The rule, as data. Index is the state byte.
 *
 * The three flat colours are deliberately far apart in both hue and lightness: a viewer must be
 * able to tell "never looked" from "looked, kept nothing" from "no longer know" at a glance, and
 * T-413 declined the obvious rendering precisely because reusing one mark for two of them spells
 * *looked and it was quiet* as *never looked*. Three states, three marks.
 */
export const CELL_MARKS: readonly CellMark[] = [
  { kind: "flat", rgb: [0.155, 0.16, 0.18] }, // UNOBSERVED — THE grey
  { kind: "ramp" }, //                           OBSERVED
  { kind: "flat", rgb: [0.1, 0.19, 0.17] }, //   NO_LEVEL — sampled, level not retained
  { kind: "flat", rgb: [0.24, 0.16, 0.26] }, //  UNKNOWN — the fourth state
];

/** The grey, named once so a test can count its occurrences in the generated shader. */
export const GREY: readonly [number, number, number] = (CELL_MARKS[CELL.UNOBSERVED] as { rgb: readonly [number, number, number] }).rgb;

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
   * fourth mark again, and never grey. */
  | "pending";

/** The mark for "we have not loaded this yet" — a memory/latency fact, visibly not the grey. */
export const PENDING: readonly [number, number, number] = [0.07, 0.075, 0.1];

/** The mark for a place no pane covers (between panes). Not a claim about anything. */
export const BACKDROP: readonly [number, number, number] = [0.04, 0.04, 0.05];

/** The rule in TypeScript, for tests and for anything that must reason about a cell off-GPU. */
export function markFor(state: number): CellMark {
  return CELL_MARKS[state] ?? CELL_MARKS[CELL.UNOBSERVED];
}

const num = (v: number) => (Number.isInteger(v) ? v.toFixed(1) : String(v));
const vec3 = (c: readonly [number, number, number]) => `vec3(${c.map(num).join(",")})`;

/**
 * The same rule as GLSL, generated from [[CELL_MARKS]]: `vec3 cellMark(int s, float x)`, where `x`
 * is the already-normalised 0…1 position on the ramp. Requires `cmap` (ui/src/cmap.ts's
 * [[CMAP_GLSL]], the one ramp) to be declared before it.
 *
 * Generated, not written out, so the grey in the shader and the grey in [[CELL_MARKS]] cannot
 * become two greys.
 */
export const CELL_RULE_GLSL: string = (() => {
  const lines = CELL_MARKS.map((m, s) => {
    const body = m.kind === "ramp" ? "cmap(x)" : vec3(m.rgb);
    return s === CELL_MARKS.length - 1 ? `  return ${body};` : `  if (s == ${s}) return ${body};`;
  });
  return `
vec3 cellMark(int s, float x) {
${lines.join("\n")}
}`;
})();
