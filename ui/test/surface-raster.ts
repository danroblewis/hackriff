// A reference rasteriser for the frames `ui/src/surface/surface.ts` submits (T-441).
//
// **Why a rasteriser and not more uniform assertions.** The thing T-441 owes is that the three
// honesty tiers, the five cell states and the stand-in mark are *distinguishable in a rendered
// frame*. A test that reads `uTier` off a draw proves the renderer passed a flag; it proves nothing
// about whether two tiers ever look different, which is the entire claim. So this walks the ops the
// stub recorded — viewports, clears and draws, with the uniforms as they stood at each draw and the
// **bytes actually uploaded** to the samplers each one read — and produces pixels.
//
// It is not a second implementation of the rule. Every colour here comes from `cellPixel`, the
// CPU-side half of `ui/src/surface/cellrule.ts`, which is the same table the fragment shader is
// *generated* from — and whose pattern tests are the same GLSL expression strings, compiled rather
// than transcribed. The geometry (clip space → window pixels, the quad's `vQ`, NEAREST sampling) is
// the vertex shader's four lines, which is the only part written twice; `surface-honesty.test.ts`
// pins the shader's own text against it.

import { CELL, cellPixel, type Rgb } from "../src/surface/cellrule";
import type { GlOp, Upload } from "./surface-glstub";

export interface Framebuffer {
  readonly w: number;
  readonly h: number;
  /** `w * h * 3`, linear RGB 0…1, origin **bottom-left** (GL's own convention). */
  readonly rgb: Float32Array;
  /** The colour at a window pixel. */
  at(x: number, y: number): [number, number, number];
  /** Every distinct colour drawn inside `rect`, quantised to 1/512, with how many pixels each. */
  histogram(rect?: Rect): Map<string, number>;
}

export interface Rect { x: number; y: number; w: number; h: number }

const KIND_FLAT = 1;

/** NEAREST + CLAMP_TO_EDGE, the only filtering the renderer sets. */
function sample(up: Upload | undefined, u: number, v: number): number {
  if (!up || !up.w || !up.h) return 0;
  const f = Math.min(up.w - 1, Math.max(0, Math.floor(u * up.w)));
  const t = Math.min(up.h - 1, Math.max(0, Math.floor(v * up.h)));
  return up.data[t * up.w + f] ?? 0;
}

/**
 * Replay a recorded frame into pixels.
 *
 * `ops` must be one frame's worth (call `g.reset()` before `render`). Clears fill the current
 * viewport, which the renderer always sets equal to the pane's scissor rect before clearing it.
 */
export function rasterize(ops: readonly GlOp[], w: number, h: number): Framebuffer {
  const rgb = new Float32Array(w * h * 3);
  let vp: Rect = { x: 0, y: 0, w, h };

  const put = (x: number, y: number, c: Rgb) => {
    if (x < 0 || y < 0 || x >= w || y >= h) return;
    const i = (y * w + x) * 3;
    rgb[i] = c[0]; rgb[i + 1] = c[1]; rgb[i + 2] = c[2];
  };

  for (const op of ops) {
    if (op.kind === "viewport") { vp = { x: op.args[0], y: op.args[1], w: op.args[2], h: op.args[3] }; continue; }
    if (op.kind === "clear") {
      const c: Rgb = [op.args[0], op.args[1], op.args[2]];
      for (let y = vp.y; y < vp.y + vp.h; y++) for (let x = vp.x; x < vp.x + vp.w; x++) put(x, y, c);
      continue;
    }
    if (op.kind !== "draw" || !op.u) continue;
    const u = op.u;
    const [cx0, cy0, cx1, cy1] = u.uRect ?? [-1, -1, 1, 1];
    // The vertex shader: gl_Position = mix(uRect.xy, uRect.zw, q), then the viewport transform.
    const sx = (c: number) => vp.x + (c * 0.5 + 0.5) * vp.w;
    const sy = (c: number) => vp.y + (c * 0.5 + 0.5) * vp.h;
    const x0 = sx(cx0), x1 = sx(cx1), y0 = sy(cy0), y1 = sy(cy1);
    const lo = { x: Math.min(x0, x1), y: Math.min(y0, y1) };
    const hi = { x: Math.max(x0, x1), y: Math.max(y0, y1) };
    const flat = (u.uKind?.[0] ?? 0) === KIND_FLAT;
    const [pw, ph] = u.uSizePx ?? [hi.x - lo.x, hi.y - lo.y];
    const srcPx = { x: (u.uSrcPx ?? [1, 1])[0], y: (u.uSrcPx ?? [1, 1])[1] };
    const tier = u.uTier?.[0] ?? 0;
    const fallback = (u.uFallback?.[0] ?? 0) > 0.5;
    const fog = (u.uFog?.[0] ?? 1) !== 0; // T-807: the pane's coverage-fog layer, as the shader reads it
    const lodb = u.uLo?.[0] ?? -120, hidb = u.uHi?.[0] ?? -60;
    const [u0, v0] = u.uUv0 ?? [0, 0];
    const [u1, v1] = u.uUv1 ?? [1, 1];
    const value = op.units?.[0], state = op.units?.[1];

    for (let y = Math.max(vp.y, Math.floor(lo.y)); y < Math.min(vp.y + vp.h, Math.ceil(hi.y)); y++) {
      for (let x = Math.max(vp.x, Math.floor(lo.x)); x < Math.min(vp.x + vp.w, Math.ceil(hi.x)); x++) {
        if (flat) { put(x, y, [u.uFlat[0], u.uFlat[1], u.uFlat[2]]); continue; }
        // `vQ`, the interpolated corner weight, from the pixel centre.
        const qx = x1 === x0 ? 0 : (x + 0.5 - x0) / (x1 - x0);
        const qy = y1 === y0 ? 0 : (y + 0.5 - y0) / (y1 - y0);
        const uu = u0 + (u1 - u0) * qx, vv = v0 + (v1 - v0) * qy;
        const s = Math.round(sample(state, uu, vv));
        const v = sample(value, uu, vv);
        put(x, y, cellPixel({
          state: s,
          x: (v - lodb) / Math.max(hidb - lodb, 1e-6),
          px: { x: qx * pw, y: qy * ph },
          tier, srcPx, fallback, fog,
        }));
      }
    }
  }

  const key = (c: readonly number[]) => c.map((v) => Math.round(v * 512)).join(",");
  return {
    w, h, rgb,
    at(x, y) { const i = (y * w + x) * 3; return [rgb[i], rgb[i + 1], rgb[i + 2]]; },
    histogram(rect) {
      const r = rect ?? { x: 0, y: 0, w, h };
      const m = new Map<string, number>();
      for (let y = r.y; y < r.y + r.h; y++) {
        for (let x = r.x; x < r.x + r.w; x++) {
          const k = key(this.at(x, y));
          m.set(k, (m.get(k) ?? 0) + 1);
        }
      }
      return m;
    },
  };
}

/** The quantised key `histogram` uses, for a colour a test names directly. */
export const colourKey = (c: readonly number[]) => c.map((v) => Math.round(v * 512)).join(",");

/** Every window pixel in `rect` whose colour is exactly `c`. */
export function countColour(fb: Framebuffer, c: Rgb, rect?: Rect): number {
  return fb.histogram(rect).get(colourKey(c)) ?? 0;
}

/** The cell states, by name, for a test that wants to say what it is looking at. */
export const STATE_NAMES: Record<number, string> = {
  [CELL.UNOBSERVED]: "unobserved",
  [CELL.OBSERVED]: "observed",
  [CELL.NO_LEVEL]: "no_level",
  [CELL.UNKNOWN]: "unknown",
  [CELL.AWAITING]: "awaiting",
};
