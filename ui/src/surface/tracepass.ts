// The trace pass (T-475): **a third program, after the data pass and beside the overlay pass**.
//
// ## Why the trace could not stay on `overlay.ts`
//
// T-457 argued that a trace can never tint a measurement *because the pass it rides has no sampler
// and no ramp*, and that argument was true and load-bearing while the trace was two flat-coloured
// series. The user's ask — **colour the trace by amplitude, from the waterfall's own ramp** — retires
// it: a series that carries a measurement colour cannot be drawn by a program incapable of producing
// one, and extending `overlay.ts` to produce one would put a ramp into the pass that draws every
// signal box, pane rectangle and lit segment on the surface. That is exactly the shape of T-397 (two
// ramps, drifting) and of the T-437 trap (a wash over the point being measured).
//
// So the trace gets a pass of its own, and the two properties that the old argument was protecting
// are re-established structurally rather than by absence of capability:
//
//   - **The data pass is untouched.** This program is used only by `view.ts`, only after
//     `Surface.render` has submitted every tile draw, and it shares no uniform, no texture unit and
//     no state with it. `ui/test/surface-trace.test.ts` asserts the data draws are byte-identical
//     with the trace on and off — unchanged from T-457, and still needing no flag.
//   - **A trace cannot be mistaken FOR tile data.** Not because it lacks a colour, but because it is
//     drawn into a rectangle **carved off** the pane (`view.ts` shortens the pane by exactly the
//     strip), scissored to that rectangle, and **has no sampler**: it cannot read a tile, and it
//     cannot express `cellrule.ts`'s grey, its tier hatching or its fallback mark. The ramp it does
//     express arrives as a **vertex attribute** computed by `trace.ts` from `ui/src/cmap.ts` — the
//     one module that defines a ramp. There are no stops in this file and no `cmap` in this shader,
//     so the repo-wide "exactly one definer" guard holds with nothing weakened.
//
// ## What it draws
//
// A mitred, feathered polyline per [[TracePath]], expanded on the CPU into one triangle strip per
// batch — one buffer upload and one draw call for every series of one pane, afterglow included.
// Anti-aliasing is a per-fragment coverage ramp across the stroke (`aEdge`), not multisampling, so it
// costs nothing and behaves the same on every context the surface runs on.

import type { TracePath } from "./trace";
import type { PaneRect } from "./surface";

type GL = WebGL2RenderingContext;

/** Floats per vertex: clip x,y · rgba · signed edge, half-width px. */
const STRIDE = 8;

const VS = `#version 300 es
precision highp float;
in vec2 aPos;    // the strip's clip space
in vec4 aRgba;   // the ramp colour of THIS vertex's dB, and the path's opacity
in vec2 aEdge;   // x: -1..1 across the stroke, y: half-width in device px
out vec4 vRgba;
out vec2 vEdge;
void main() {
  vRgba = aRgba;
  vEdge = aEdge;
  gl_Position = vec4(aPos, 0.0, 1.0);
}`;

// No sampler, no ramp, no cell state: this shader can neither read a measurement nor invent one of
// `cellrule.ts`'s marks. Its colour is whatever `trace.ts` put on the vertex.
const FS = `#version 300 es
precision highp float;
in vec4 vRgba;
in vec2 vEdge;
out vec4 frag;
void main() {
  // One device pixel of coverage ramp at each edge of the stroke. |vEdge.x| is 1 at the edge, so
  // (1 - |e|) * halfPx is the distance to it in pixels, clamped to full coverage in the core.
  float cov = clamp((1.0 - abs(vEdge.x)) * vEdge.y, 0.0, 1.0);
  frag = vec4(vRgba.rgb, vRgba.a * cov);
}`;

function compile(gl: GL, type: number, src: string): WebGLShader {
  const s = gl.createShader(type)!;
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(`${gl.getShaderInfoLog(s)}\n${src}`);
  return s;
}

/**
 * Expand one path into triangle-strip vertices, appended to `out`.
 *
 * Mitred in **device-pixel space** — the strip is far wider than it is tall, so a normal computed in
 * clip space would be squashed and a steep segment would come out hairline-thin. The miter is
 * clamped at [[MITER_LIMIT]]: at a near-vertical spike the exact miter runs away to infinity, and a
 * clamped one reads as a bevel, which is what every stroker does and what nobody notices.
 *
 * Returns the number of vertices appended.
 */
const MITER_LIMIT = 3;

function expand(p: TracePath, rect: PaneRect, out: number[]): number {
  const n = p.xy.length >> 1;
  if (n < 2 || !(rect.w > 0) || !(rect.h > 0)) return 0;
  const sx = rect.w / 2, sy = rect.h / 2;           // clip → device px
  const half = Math.max(0.5, p.widthPx / 2);
  const px = new Float64Array(n), py = new Float64Array(n);
  for (let i = 0; i < n; i++) { px[i] = p.xy[2 * i] * sx; py[i] = p.xy[2 * i + 1] * sy; }
  let count = 0;
  const vert = (i: number, side: number, nx: number, ny: number, scale: number) => {
    const x = (px[i] + nx * half * scale) / sx;
    const y = (py[i] + ny * half * scale) / sy;
    out.push(x, y, p.rgb[3 * i], p.rgb[3 * i + 1], p.rgb[3 * i + 2], p.alpha, side, half);
    count++;
  };
  for (let i = 0; i < n; i++) {
    // The segment normals either side of this point; at an end there is only one.
    let ax = 0, ay = 0, bx = 0, by = 0;
    if (i > 0) {
      const dx = px[i] - px[i - 1], dy = py[i] - py[i - 1];
      const l = Math.hypot(dx, dy) || 1;
      ax = -dy / l; ay = dx / l;
    }
    if (i < n - 1) {
      const dx = px[i + 1] - px[i], dy = py[i + 1] - py[i];
      const l = Math.hypot(dx, dy) || 1;
      bx = -dy / l; by = dx / l;
    }
    if (i === 0) { ax = bx; ay = by; }
    if (i === n - 1) { bx = ax; by = ay; }
    let mx = ax + bx, my = ay + by;
    const ml = Math.hypot(mx, my);
    let scale = 1;
    if (ml > 1e-6) {
      mx /= ml; my /= ml;
      // 1/cos(θ/2): the miter has to reach further the sharper the turn.
      scale = Math.min(MITER_LIMIT, 1 / Math.max(1e-3, mx * ax + my * ay));
    } else { mx = ax; my = ay; }
    vert(i, -1, -mx, -my, scale);
    vert(i, +1, mx, my, scale);
  }
  return count;
}

/** Draws [[TracePath]]s into one pane's trace strip. Holds one growable buffer and no other state. */
export class TracePass {
  private readonly prog: WebGLProgram;
  private readonly vbo: WebGLBuffer | null;
  private readonly vao: WebGLVertexArrayObject | null;
  private readonly attrs: { pos: number; rgba: number; edge: number };
  private scratch = new Float32Array(0);
  drawCalls = 0;

  constructor(private readonly gl: GL) {
    const p = gl.createProgram()!;
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, VS));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(String(gl.getProgramInfoLog(p)));
    this.prog = p;
    this.vbo = gl.createBuffer();
    this.vao = gl.createVertexArray();
    const loc = (name: string) => {
      const l = gl.getAttribLocation(p, name);
      return typeof l === "number" ? l : -1;
    };
    this.attrs = { pos: loc("aPos"), rgba: loc("aRgba"), edge: loc("aEdge") };
    if (this.vao && this.attrs.pos >= 0) {
      gl.bindVertexArray(this.vao);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo);
      const b = STRIDE * 4;
      for (const [at, size, off] of [
        [this.attrs.pos, 2, 0], [this.attrs.rgba, 4, 8], [this.attrs.edge, 2, 24],
      ] as const) {
        if (at < 0) continue;
        gl.enableVertexAttribArray(at);
        gl.vertexAttribPointer(at, size, gl.FLOAT, false, b, off);
      }
      gl.bindVertexArray(null);
    }
  }

  /**
   * Submit `paths` inside `rect`, in order — later paths draw over earlier ones, which is how the
   * afterglow sits behind the current slice.
   *
   * Scissored to the strip, so a trace can never paint on the pane it is a trace of, and blended
   * straight (`SRC_ALPHA`) rather than additively: an additive glow would brighten wherever two
   * shadows crossed and the crossing would read as energy that is not there.
   *
   * Returns the number of vertices drawn — 0 when nothing was, which is the honest answer for a
   * window nothing has answered for.
   */
  draw(rect: PaneRect, paths: readonly TracePath[]): number {
    const gl = this.gl;
    if (!(rect.w > 0) || !(rect.h > 0) || paths.length === 0) return 0;
    const buf: number[] = [];
    // Every path of this pane — max-hold, each afterglow row, the glow, the current slice — is
    // expanded into ONE buffer and uploaded once; each is then its own `drawArrays` range, because a
    // triangle strip cannot cross a gap and the gaps are exactly what the honesty rule is about.
    const spans: { first: number; count: number }[] = [];
    for (const p of paths) {
      const first = buf.length / STRIDE;
      const n = expand(p, rect, buf);
      if (n > 0) spans.push({ first, count: n });
    }
    if (!spans.length) return 0;
    if (this.scratch.length < buf.length) this.scratch = new Float32Array(buf.length * 2);
    this.scratch.set(buf);
    gl.useProgram(this.prog);
    gl.viewport(rect.x, rect.y, rect.w, rect.h);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(rect.x, rect.y, rect.w, rect.h);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.bindVertexArray(this.vao);
    gl.bindBuffer(gl.ARRAY_BUFFER, this.vbo);
    gl.bufferData(gl.ARRAY_BUFFER, this.scratch.subarray(0, buf.length), gl.DYNAMIC_DRAW);
    let drawn = 0;
    for (const s of spans) {
      gl.drawArrays(gl.TRIANGLE_STRIP, s.first, s.count);
      drawn += s.count;
      this.drawCalls++;
    }
    gl.bindVertexArray(null);
    gl.disable(gl.BLEND);
    gl.disable(gl.SCISSOR_TEST);
    return drawn;
  }
}
