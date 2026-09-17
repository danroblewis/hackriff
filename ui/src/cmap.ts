// The waterfall's colour ramp, defined **once** (T-397/T-405/T-411).
//
// Why this file exists: the ramp lived only inside the WebGL fragment shader
// (`waterfall.ts`'s `vec3 cmap(float x)`), so the two edge navigators — which draw on a 2D canvas,
// not in GL — grew a ramp of their own. Theirs ran dark-teal → cyan and stopped there, so the same
// energy that is yellow in the main waterfall was cyan on the strips beside it, and a peak could
// never *look* like a peak however strong it was. The user reported it as "the strips are cyan and
// the yellow/red peaks are missing".
//
// Copying the six stops into the canvas path would have been the third place the ramp lives, and
// two places already disagreed. So the stops live here and both renderers consume them: the canvas
// path calls [[cmapBytes]], and the shader is handed [[CMAP_GLSL]], which is generated from the
// same array. There is no way to change one and not the other.
//
// **This file is presentation only.** A colour ramp maps an already-normalised 0…1 position to a
// colour; *what* 0 and 1 mean is a measurement and stays on the wire (`/api/coverage`'s
// `shade.range_db` + `shade.normalisation`, `/api/timeline`'s `range_db`). Nothing here reads a
// level, a unit or a scale — pass it the number the backend already normalised.

/** One stop of the ramp: the position it sits at, and its colour as linear RGB 0…1. */
export type CmapStop = readonly [number, readonly [number, number, number]];

/**
 * The ramp: near-black → deep blue → cyan → yellow → red → white.
 *
 * Positions and colours are exactly those the waterfall shader has used since spike S3. Changing
 * them changes both renderers together, which is the point of the file.
 */
export const CMAP_STOPS: readonly CmapStop[] = [
  [0, [0, 0, 0.04]],
  [0.2, [0.05, 0.1, 0.55]],
  [0.45, [0, 0.7, 0.9]],
  [0.7, [0.95, 0.9, 0.1]],
  [0.9, [0.95, 0.2, 0.05]],
  [1, [1, 1, 1]],
];

/** The ramp at `x`, clamped to 0…1, as linear RGB 0…1 — the exact arithmetic the shader does
 * (a linear mix between the bracketing stops, with the last segment catching everything above the
 * penultimate stop). A non-finite `x` reads as the bottom of the ramp, never as a wrap-around. */
export function cmap(x: number): [number, number, number] {
  const t = Number.isFinite(x) ? Math.min(1, Math.max(0, x)) : 0;
  const last = CMAP_STOPS.length - 2;
  for (let i = 0; i <= last; i++) {
    const [a, ca] = CMAP_STOPS[i];
    const [b, cb] = CMAP_STOPS[i + 1];
    if (t < b || i === last) {
      const f = b > a ? (t - a) / (b - a) : 0;
      return [ca[0] + (cb[0] - ca[0]) * f, ca[1] + (cb[1] - ca[1]) * f, ca[2] + (cb[2] - ca[2]) * f];
    }
  }
  return [0, 0, 0];
}

/** The ramp at `x` as 0…255 bytes, for `ImageData` — what the two navigator strips draw with. */
export function cmapBytes(x: number): [number, number, number] {
  const [r, g, b] = cmap(x);
  return [Math.round(255 * r), Math.round(255 * g), Math.round(255 * b)];
}

/** A GLSL float literal: GLSL has no implicit int→float, so `1` must be written `1.0`. */
const glslNum = (v: number) => (Number.isInteger(v) ? v.toFixed(1) : String(v));
const glslVec = (c: readonly [number, number, number]) =>
  `vec3(${c.map(glslNum).join(",")})`;

/**
 * The same ramp as a GLSL function `vec3 cmap(float x)`, generated from [[CMAP_STOPS]] and injected
 * into the waterfall's fragment shaders.
 *
 * The chain is `if (x < b) return mix(prev, next, (x - a)/(b - a));` per segment with the final
 * segment as the fallthrough — which is what [[cmap]] evaluates, so the canvas strips and the GL
 * waterfall produce the same colour for the same input.
 */
export const CMAP_GLSL: string = (() => {
  const decls = CMAP_STOPS.map((s, i) => `c${i}=${glslVec(s[1])}`).join(", ");
  const lines: string[] = [];
  for (let i = 0; i < CMAP_STOPS.length - 1; i++) {
    const [a] = CMAP_STOPS[i];
    const [b] = CMAP_STOPS[i + 1];
    const mix = `mix(c${i},c${i + 1},(x-${glslNum(a)})/${glslNum(b - a)})`;
    lines.push(i === CMAP_STOPS.length - 2 ? `  return ${mix};` : `  if(x<${glslNum(b)}) return ${mix};`);
  }
  return `
vec3 cmap(float x){ x = clamp(x,0.0,1.0);
  vec3 ${decls};
${lines.join("\n")} }`;
})();
