// T-437 spike — ONE WebGL2 context, N scissored panes.
//
// docs/16 §8.3. Every pane (including the minimap, and including what used to be the two
// edge navigators) is drawn by THIS program, from THIS colormap, through THIS tile cache.
// That is the whole anti-divergence argument: T-397 (navigator colormap != waterfall
// colormap) and T-411 (navigator buffer resolution != bar size) were possible only because
// the strips were canvas-2D with their own ramp and their own pooling. Here there is
// exactly one ramp function and exactly one "one cell per pixel" rule, so the two cannot
// disagree — not by discipline, but because there is only one of each.

export const TIER = { UNOBSERVED: 0, SURVEY: 1, HISTORY: 2, LIVE: 3 };

const VS = `#version 300 es
precision highp float;
uniform vec4 uRect;   // x0,y0,x1,y1 in clip space
out vec2 vUv;
void main() {
  // two triangles from gl_VertexID, no buffers
  vec2 c = vec2((gl_VertexID & 1), (gl_VertexID >> 1) & 1);
  if (gl_VertexID > 2) c = vec2(1.0, 1.0) - vec2(((5 - gl_VertexID) & 1), ((5 - gl_VertexID) >> 1) & 1);
  vUv = c;
  gl_Position = vec4(mix(uRect.xy, uRect.zw, c), 0.0, 1.0);
}`;

// One ramp. Approximates the repo's CMAP_STOPS shape (black -> blue -> magenta -> orange
// -> white); the exact stops do not matter to the spike, the SINGLE DEFINITION does.
const FS = `#version 300 es
precision highp float;
precision highp sampler2D;
in vec2 vUv;
out vec4 frag;
uniform sampler2D uTile;
uniform vec2 uUv0, uUv1;     // sub-rect of the tile this quad shows
uniform int  uMode;          // 0 = tile, 1 = flat fill, 2 = flat stroke-ish
uniform vec4 uFlat;
uniform vec2 uPxPerCell;     // screen px per tile cell, for the survey hatch
uniform float uGain;

const vec3 GREY = vec3(0.155, 0.160, 0.180);   // genuinely unobserved

vec3 ramp(float t) {
  t = clamp(t, 0.0, 1.0);
  vec3 c0 = vec3(0.02,0.02,0.09), c1 = vec3(0.12,0.10,0.45), c2 = vec3(0.55,0.11,0.53);
  vec3 c3 = vec3(0.88,0.32,0.25), c4 = vec3(0.99,0.79,0.28), c5 = vec3(1.0,1.0,0.94);
  if (t < 0.2) return mix(c0,c1,t/0.2);
  if (t < 0.4) return mix(c1,c2,(t-0.2)/0.2);
  if (t < 0.6) return mix(c2,c3,(t-0.4)/0.2);
  if (t < 0.8) return mix(c3,c4,(t-0.6)/0.2);
  return mix(c4,c5,(t-0.8)/0.2);
}

void main() {
  if (uMode != 0) { frag = uFlat; return; }
  vec2 uv = mix(uUv0, uUv1, vUv);
  vec3 s = texture(uTile, uv).rgb;
  float value = s.r;
  float duty  = s.g;
  int tier = int(floor(s.b * 255.0 + 0.5));

  // THE RULE: no coverage means unobserved, whatever the value byte happens to hold.
  if (tier == 0 || duty <= 0.0) { frag = vec4(GREY, 1.0); return; }

  vec3 col = ramp(value * uGain);

  if (tier == 1) {
    // survey-overview: desaturated toward grey + a coarse diagonal hatch, so a wide zoom
    // can never be mistaken for live detail.
    col = mix(col, GREY, 0.42);
    vec2 px = vUv * uPxPerCell * 256.0;
    float h = fract((px.x + px.y) / 9.0);
    if (h < 0.34) col *= 0.74;
  } else if (tier == 2) {
    // spectrum-history: full ramp, slightly dimmed, no hatch.
    col *= 0.88;
  } else {
    // live-iq: full ramp, lifted, plus a faint scanline so "this is the growing edge" reads.
    col = min(col * 1.10 + 0.02, vec3(1.0));
  }
  // partial coverage darkens proportionally — duty < 1 IS the partial-coverage statement
  col *= mix(0.45, 1.0, duty);
  frag = vec4(col, 1.0);
}`;

function compile(gl, type, src) {
  const s = gl.createShader(type);
  gl.shaderSource(s, src); gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) + "\n" + src);
  return s;
}

export class Surface {
  constructor(canvas) {
    const gl = canvas.getContext("webgl2", { antialias: false, alpha: false, preserveDrawingBuffer: true });
    if (!gl) throw new Error("WebGL2 unavailable");
    this.gl = gl; this.canvas = canvas;
    const p = gl.createProgram();
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, VS));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
    this.prog = p;
    this.u = {};
    for (const n of ["uRect", "uTile", "uUv0", "uUv1", "uMode", "uFlat", "uPxPerCell", "uGain"])
      this.u[n] = gl.getUniformLocation(p, n);
    this.vao = gl.createVertexArray();
    this.drawCalls = 0;
    this.scissorSets = 0;
    this.contexts = 1; // asserted by the tests: exactly one, forever
  }

  begin() {
    const gl = this.gl;
    gl.bindVertexArray(this.vao);
    gl.useProgram(this.prog);
    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.SCISSOR_TEST);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.clearColor(0.06, 0.06, 0.07, 1); // between panes
    gl.clear(gl.COLOR_BUFFER_BIT);
    this.drawCalls = 0; this.scissorSets = 0;
  }

  /**
   * Bind one pane's pixel rectangle. viewport + scissor is what makes N panes share one
   * context and therefore one cache. Returns clip-space helpers for that pane.
   */
  bindPane(rect) {
    const gl = this.gl;
    gl.viewport(rect.x, rect.y, rect.w, rect.h);
    gl.enable(gl.SCISSOR_TEST);
    gl.scissor(rect.x, rect.y, rect.w, rect.h);
    this.scissorSets++;
    // Grey the whole pane first: anything we have no tile for stays honestly unobserved.
    gl.clearColor(0.155, 0.160, 0.180, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
  }

  /** Draw one tile into the bound pane. `rect` and `uv` are already normalised. */
  drawTile(tex, clipRect, uv0, uv1, pxPerCell, gain = 1) {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.uniform1i(this.u.uTile, 0);
    gl.uniform1i(this.u.uMode, 0);
    gl.uniform4f(this.u.uRect, clipRect[0], clipRect[1], clipRect[2], clipRect[3]);
    gl.uniform2f(this.u.uUv0, uv0[0], uv0[1]);
    gl.uniform2f(this.u.uUv1, uv1[0], uv1[1]);
    gl.uniform2f(this.u.uPxPerCell, pxPerCell[0], pxPerCell[1]);
    gl.uniform1f(this.u.uGain, gain);
    gl.drawArrays(gl.TRIANGLES, 0, 6);
    this.drawCalls++;
  }

  drawFlat(clipRect, rgba) {
    const gl = this.gl;
    gl.uniform1i(this.u.uMode, 1);
    gl.uniform4f(this.u.uRect, clipRect[0], clipRect[1], clipRect[2], clipRect[3]);
    gl.uniform4f(this.u.uFlat, rgba[0], rgba[1], rgba[2], rgba[3]);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.drawArrays(gl.TRIANGLES, 0, 6);
    gl.disable(gl.BLEND);
    this.drawCalls++;
  }

  /** Hollow rectangle from four thin flats — pane-viewport rects on the minimap. */
  strokeRect(clipRect, rgba, thickPx, paneRect) {
    const [x0, y0, x1, y1] = clipRect;
    const tx = (2 * thickPx) / paneRect.w, ty = (2 * thickPx) / paneRect.h;
    this.drawFlat([x0, y0, x1, y0 + ty], rgba);
    this.drawFlat([x0, y1 - ty, x1, y1], rgba);
    this.drawFlat([x0, y0, x0 + tx, y1], rgba);
    this.drawFlat([x1 - tx, y0, x1, y1], rgba);
  }

  end() { this.gl.disable(this.gl.SCISSOR_TEST); }
}

/** Map an absolute box onto a pane's clip space. Pure arithmetic; the one time axis. */
export function toClip(box, view) {
  const fx = (f) => 2 * ((f - view.f0) / (view.f1 - view.f0)) - 1;
  // Time runs DOWN the pane: t0 (oldest) at the bottom, live edge at the top.
  const ty = (t) => 2 * ((t - view.t0) / (view.t1 - view.t0)) - 1;
  return [fx(box.f0), ty(box.t0), fx(box.f1), ty(box.t1)];
}
