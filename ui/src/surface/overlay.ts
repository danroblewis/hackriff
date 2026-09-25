// The overlay pass (T-443): **a separate program, a separate pass, run after the data pass**.
//
// Why it is not a branch of the tile shader. The T-437 spike's first minimap comparison failed
// because a translucent pane-viewport wash was drawn over the sample point — an overlay tinting a
// measurement, inside the spike that existed to prove measurements are not tinted. The spike's fix
// was `setOverlays(false)` before measuring, i.e. a flag someone must remember. The fix here is
// structural instead:
//
//   - **This pass cannot reach the data pass.** `Surface.render` never calls it; a caller runs it
//     afterwards, with its own program, and the tile draws have already been submitted by then. A
//     test asserts the data draws are byte-identical with overlays on and off, so the honesty
//     comparison the spike wanted needs no flag at all.
//   - **This program has no ramp and no cell state.** Its only uniforms are a rectangle and a flat
//     colour: there is no expression in it that can produce a measurement colour or a grey. The one
//     ramp stays `CMAP_GLSL`'s and the one grey stays `CELL_RULE_GLSL`'s (T-397/T-440's pattern).
//   - **Only strokes are submitted.** Geometry comes from ui/src/surface/minimap.ts, which emits
//     rectangle *edges* and a thin bar — nothing that covers the interior of a region.
//   - **T-910: a pattern is still a stroke.** GIS symbology asks for a dashed outline (a Candidate)
//     and a light fill (a Confirmed box). Both are cut by this shader from one quad with a
//     screen-door [[OverlayPattern]]: fragments off the pattern are *discarded*, so every pixel
//     between two hatch lines or two dashes is the measurement, untouched — never a translucent
//     wash over it. The pattern's inputs are a mode, two lengths and an anchor; there is still no
//     sampler, no ramp and no cell state for it to express a measurement with.
//
// The flag still exists, because a user may want a bare map; it is just not what keeps the surface
// honest.

import type { OverlayQuad, OverlayPattern } from "./minimap";
import type { PaneRect } from "./surface";

type GL = WebGL2RenderingContext;

const VS = `#version 300 es
precision highp float;
uniform vec4 uQuad; // x0,y0,x1,y1 in the pane's clip space
void main() {
  vec2 q = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
  gl_Position = vec4(mix(uQuad.xy, uQuad.zw, q), 0.0, 1.0);
}`;

// No sampler, no ramp, no state byte: this shader is incapable of drawing a measurement. `uPat` is
// T-910's screen-door pattern (mode, period px, on px): off-pattern fragments are DISCARDED, never
// blended, so what is between two hatch lines or two dashes is the data exactly as the data pass
// drew it. `uPatOrigin` anchors the pattern to the feature (window px), so it moves with the box.
const FS = `#version 300 es
precision highp float;
out vec4 frag;
uniform vec4 uInk;
uniform vec4 uPat;
uniform vec2 uPatOrigin;
void main() {
  if (uPat.x > 0.5) {
    vec2 d = gl_FragCoord.xy - uPatOrigin;
    float s = uPat.x < 1.5 ? d.x - d.y : (uPat.x < 2.5 ? d.x : d.y);
    if (mod(s, uPat.y) >= uPat.z) discard;
  }
  frag = uInk;
}`;

/** `uPat.x` per [[OverlayPattern]] mode; 0 = a plain stroke. */
const PATTERN_MODE = { hatch: 1, "dash-x": 2, "dash-y": 3 } as const;

function compile(gl: GL, type: number, src: string): WebGLShader {
  const s = gl.createShader(type)!;
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(`${gl.getShaderInfoLog(s)}\n${src}`);
  return s;
}

/** Draws [[OverlayQuad]]s into one pane's rectangle. Holds no state about what it drew. */
export class OverlayPass {
  private readonly prog: WebGLProgram;
  private readonly uQuad: WebGLUniformLocation | null;
  private readonly uInk: WebGLUniformLocation | null;
  private readonly uPat: WebGLUniformLocation | null;
  private readonly uPatOrigin: WebGLUniformLocation | null;
  drawCalls = 0;

  constructor(private readonly gl: GL) {
    const p = gl.createProgram()!;
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, VS));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(String(gl.getProgramInfoLog(p)));
    this.prog = p;
    this.uQuad = gl.getUniformLocation(p, "uQuad");
    this.uInk = gl.getUniformLocation(p, "uInk");
    this.uPat = gl.getUniformLocation(p, "uPat");
    this.uPatOrigin = gl.getUniformLocation(p, "uPatOrigin");
  }

  /**
   * Submit `quads` inside `rect`. Scissored to the rectangle, so an overlay for one viewport can
   * never paint over another one, and blended so a mark can be translucent without the pass owning
   * any notion of what is underneath.
   */
  draw(rect: PaneRect, quads: readonly OverlayQuad[]): number {
    const gl = this.gl;
    if (!(rect.w > 0) || !(rect.h > 0) || quads.length === 0) return 0;
    gl.useProgram(this.prog);
    gl.viewport(rect.x, rect.y, rect.w, rect.h);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(rect.x, rect.y, rect.w, rect.h);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    let n = 0;
    for (const q of quads) {
      gl.uniform4f(this.uQuad, q.clip[0], q.clip[1], q.clip[2], q.clip[3]);
      gl.uniform4f(this.uInk, q.rgba[0], q.rgba[1], q.rgba[2], q.rgba[3]);
      const pat: OverlayPattern | undefined = q.pattern;
      if (pat) {
        gl.uniform4f(this.uPat, PATTERN_MODE[pat.mode], Math.max(1, pat.periodPx), Math.max(0, pat.onPx), 0);
        gl.uniform2f(this.uPatOrigin, rect.x + ((pat.origin[0] + 1) / 2) * rect.w, rect.y + ((pat.origin[1] + 1) / 2) * rect.h);
      } else {
        gl.uniform4f(this.uPat, 0, 1, 1, 0);
      }
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
      n++;
    }
    gl.disable(gl.BLEND);
    gl.disable(gl.SCISSOR_TEST);
    this.drawCalls += n;
    return n;
  }
}
