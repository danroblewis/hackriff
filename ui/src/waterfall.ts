// WebGL2 waterfall + spectrum line + GPU DPX persistence, ported from spike S3
// (spikes/s3-web-waterfall/client/src/main.ts, measured at 60 fps up to 16384 bins).
// Differences: rows arrive as f32 dB (hk-stream `rf32_le` spectrum), stored in an R32F texture
// ring and mapped to colour in the shader, so auto-ranging re-maps history for free; a left-edge
// strip marks dropped/gated runs; a texture window [u0, u1] zooms every pass (T-044); row times
// are kept for time selections.
//
// Frequency geometry (T-045, ui/src/axis.ts): texture column j of texW covers [j/texW, (j+1)/texW]
// of the full band, so a bin's centre is at u = (i + 0.5)/N. The spectrum line puts its vertices
// there (not at i/(N−1), which skewed the trace by up to a bin towards the edges).
//
// T-051: each screen pixel max-pools the texels under its own footprint, centred on the pixel
// (`axis.poolWindow`, mirrored by POOL below; previously [x0, x0+step), ~1 px biased); colour
// scale auto or manual; peak (max) hold trace.
//
// T-152 (additive, MUI centre): `reset()`, `setRows()` bulk history load, `texWidth`, `lineColor`,
// the UNOBSERVED_DB grey sentinel, and `uploadMs`/`frameMs` frame-time counters.
//
// T-337 (the user's "one shared time axis" invariant): every row keeps the absolute capture time
// the backend served with it, and `timeAt`/`rowsBackAt` are the one mapping between capture time
// and screen position that overlays place themselves through.

import { rowsBackAt } from "./axis";

const ROWS = 512;
const LEVELS = 256;
const TAU_S = 0.5;
const MAX_ROWS_PER_FRAME = 16;
/** Fraction of the canvas height the spectrum takes at the top. */
const SPEC_FRAC = 0.35;
export const MARK_DROP = 1;
export const MARK_GATED = 2;
/** A cell value meaning "not observed" (T-152 review render: history `null` cells). Drawn grey, never
 * as quiet; ignored by the auto colour range. Far below any real dB level. */
export const UNOBSERVED_DB = -1e20;

/** Rows the waterfall ring holds, and so how much time is on screen: `WATERFALL_ROWS / rowRateHz`
 * seconds (T-260, ADR-0017 §2.1). Exported so Explore can scope its Candidate query to the window
 * the user is actually looking at without importing the renderer. Additive — the ring size itself
 * is unchanged. */
export const WATERFALL_ROWS = ROWS;

/** One row max-decimated (or nearest-stretched) to `texW` texels: what `push` uploads. Pure, so the
 * row-preparation cost is measured in node (ui/test/app-centre.test.ts). */
export function decimateRow(db: Float32Array, texW: number): Float32Array {
  if (db.length === texW) return db.slice();
  const row = new Float32Array(texW);
  const r = db.length / texW;
  for (let i = 0; i < texW; i++) {
    let m = -Infinity;
    const s = Math.floor(i * r), e = Math.min(db.length, Math.max(s + 1, Math.floor((i + 1) * r)));
    for (let j = s; j < e; j++) if (db[j] > m) m = db[j];
    row[i] = m;
  }
  return row;
}

const VS_FULL = `#version 300 es
out vec2 vUv;
void main(){ vec2 p = vec2((gl_VertexID<<1)&2, gl_VertexID&2); vUv = p; gl_Position = vec4(p*2.0-1.0,0,1); }`;
const CMAP = `
vec3 cmap(float x){ x = clamp(x,0.0,1.0);
  vec3 c0=vec3(0.0,0.0,0.04), c1=vec3(0.05,0.1,0.55), c2=vec3(0.0,0.7,0.9), c3=vec3(0.95,0.9,0.1), c4=vec3(0.95,0.2,0.05), c5=vec3(1.0);
  if(x<0.2) return mix(c0,c1,x/0.2); if(x<0.45) return mix(c1,c2,(x-0.2)/0.25);
  if(x<0.7) return mix(c2,c3,(x-0.45)/0.25); if(x<0.9) return mix(c3,c4,(x-0.7)/0.2); return mix(c4,c5,(x-0.9)/0.1); }`;
// Texels [x0, x0+n) under a pixel centred at u that spans pxU of the band (axis.ts poolWindow).
const POOL = `
ivec2 poolWin(float u, float pxU, int w){ float fw = float(w); float a = (u-0.5*pxU)*fw, b = (u+0.5*pxU)*fw;
  if (!(b - a > 1.0)) return ivec2(int(floor(u*fw)), 1);
  int x0 = int(floor(a)); int n = max(1, int(ceil(b)) - x0);
  if (n > 64) { x0 = int(floor(0.5*(a+b))) - 32; n = 64; }
  return ivec2(x0, n); }`;

type Prog = { p: WebGLProgram; u: Record<string, WebGLUniformLocation | null> };

export class Waterfall {
  readonly bins: number;
  readonly rows = ROWS;
  lo = -120;
  hi = -60;
  fps = 0;
  skipped = 0;
  latest: Float32Array;
  /** Fraction of the canvas height taken by the spectrum (the rest is the waterfall). */
  specFrac = SPEC_FRAC;
  /** Spectrum line colour (RGB 0..1). */
  lineColor: [number, number, number] = [1.0, 1.0, 0.6];
  /** Smoothed CPU time (ms) of one frame's row uploads, and of the whole frame (T-152 perf check). */
  uploadMs = 0;
  frameMs = 0;
  private gl: WebGL2RenderingContext;
  private texW: number;
  private wf: WebGLTexture;
  private mk: WebGLTexture;
  private hist: { tex: WebGLTexture; fb: WebGLFramebuffer }[] = [];
  private progs: Record<string, Prog> = {};
  private head = 0;
  private pending: { row: Float32Array; mark: number; t: number }[] = [];
  private times = new Float64Array(ROWS).fill(NaN);
  private filledHead = -1;
  private filledN = 0;
  private nextMark = 0;
  private beta: number;
  private floorEst = NaN;
  private peakEst = NaN;
  private rafTimes: number[] = [];
  private raf = 0;
  private scratch: Float32Array;
  private u0 = 0;
  private u1 = 1;
  /** Colour scale follows the floor/peak estimate (false: `lo`/`hi` set by the user). */
  private autoScale = true;
  private peakHold = false;
  private peak: Float32Array | null = null;
  private peakDirty = false;
  private peakTex: WebGLTexture;

  constructor(private canvas: HTMLCanvasElement, bins: number, rowRateHz: number) {
    const gl = canvas.getContext("webgl2", { antialias: false, alpha: false });
    if (!gl) throw new Error("WebGL2 unavailable");
    this.gl = gl;
    this.bins = bins;
    this.texW = Math.min(bins, gl.getParameter(gl.MAX_TEXTURE_SIZE) as number);
    this.latest = new Float32Array(this.texW).fill(NaN);
    this.scratch = new Float32Array(Math.min(this.texW, 512));
    this.beta = Math.exp(-1 / (TAU_S * Math.max(1, rowRateHz)));
    this.buildPrograms();
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    this.wf = this.texture(gl.R32F, this.texW, ROWS);
    // Rows not yet received read as "far below range" (dark), not 0 dB (white).
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, this.texW, ROWS, gl.RED, gl.FLOAT, new Float32Array(this.texW * ROWS).fill(-1e30));
    this.mk = this.texture(gl.R8, 1, ROWS);
    this.peakTex = this.texture(gl.R32F, this.texW, 1);
    if (gl.getExtension("EXT_color_buffer_float") || gl.getExtension("EXT_color_buffer_half_float")) {
      for (let i = 0; i < 2; i++) {
        const tex = this.texture(gl.R16F, this.texW, LEVELS);
        const fb = gl.createFramebuffer()!;
        gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
        gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
        gl.clearColor(0, 0, 0, 0);
        gl.clear(gl.COLOR_BUFFER_BIT);
        this.hist.push({ tex, fb });
      }
      gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    }
    gl.bindVertexArray(gl.createVertexArray());
    this.raf = requestAnimationFrame((t) => this.frame(t));
  }

  destroy() {
    cancelAnimationFrame(this.raf);
    this.gl.getExtension("WEBGL_lose_context")?.loseContext();
  }

  /** Texture width in texels (≤ bins): the row length history renders resample to. */
  get texWidth(): number { return this.texW; }

  /** Clears every row, time, mark, persistence and the peak trace (a retune or a switch between
   * live and reviewing): the ring reads as "not received" again. */
  reset() {
    const gl = this.gl;
    this.pending = [];
    this.times.fill(NaN);
    this.head = 0;
    this.filledHead = -1;
    this.nextMark = 0;
    this.latest = new Float32Array(this.texW).fill(NaN);
    gl.bindTexture(gl.TEXTURE_2D, this.wf);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, this.texW, ROWS, gl.RED, gl.FLOAT, new Float32Array(this.texW * ROWS).fill(-1e30));
    gl.bindTexture(gl.TEXTURE_2D, this.mk);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, 1, ROWS, gl.RED, gl.UNSIGNED_BYTE, new Uint8Array(ROWS));
    for (const hb of this.hist) {
      gl.bindFramebuffer(gl.FRAMEBUFFER, hb.fb);
      gl.clearColor(0, 0, 0, 0);
      gl.clear(gl.COLOR_BUFFER_BIT);
    }
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    this.resetPeak();
  }

  /** Replaces the ring with `rows` (oldest first, at most `rows` kept, newest on top) and their
   * times, uploaded at once rather than through the per-frame queue: a history render (T-152). */
  setRows(rows: readonly Float32Array[], times: readonly number[]) {
    this.reset();
    const gl = this.gl, from = Math.max(0, rows.length - ROWS);
    gl.bindTexture(gl.TEXTURE_2D, this.wf);
    for (let k = from; k < rows.length; k++) {
      const row = decimateRow(rows[k], this.texW);
      this.autoRange(row);
      this.head = (this.head + 1) % ROWS;
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, this.head, this.texW, 1, gl.RED, gl.FLOAT, row);
      this.times[this.head] = times[k] ?? NaN;
      this.latest = row;
    }
  }

  /** Shows the texture window [u0, u1] of the full band (0..1): the zoom. */
  setView(u0: number, u1: number) {
    if (Number.isFinite(u0) && Number.isFinite(u1) && u1 > u0) { this.u0 = u0; this.u1 = u1; }
  }

  /** Auto colour scale, or a manual [lo, hi] in dB (ignored unless lo < hi). */
  setScale(auto: boolean, lo?: number, hi?: number) {
    this.autoScale = auto;
    if (auto) {
      if (!Number.isNaN(this.floorEst)) { this.lo = this.floorEst - 8; this.hi = Math.max(this.floorEst + 30, this.peakEst + 3); }
    } else if (lo !== undefined && hi !== undefined && Number.isFinite(lo) && Number.isFinite(hi) && hi > lo) {
      this.lo = lo;
      this.hi = hi;
    }
  }

  /** Peak (max) hold trace over the spectrum; turning it on starts afresh. */
  setPeakHold(on: boolean) {
    if (on === this.peakHold) return;
    this.peakHold = on;
    this.resetPeak();
  }

  resetPeak() {
    this.peak = null;
    this.peakDirty = false;
  }

  /** Marks the next row (a dropped or gated run precedes it). */
  mark(kind: number) {
    this.nextMark = Math.max(this.nextMark, kind);
  }

  /** Time (Unix s) of the row `rowsBack` rows before the newest drawn row; NaN when none. */
  timeAt(rowsBack: number): number {
    if (!(rowsBack >= 0 && rowsBack < ROWS)) return NaN;
    return this.times[(this.head - Math.floor(rowsBack) + ROWS) % ROWS];
  }

  /**
   * The canonical time→screen mapping (T-337): absolute capture time (Unix s) → rows-back,
   * fractional, and the **exact inverse of [[timeAt]]** at every integer row (`axis.rowsBackAt`).
   * Overlays place themselves through this, so a box sits on the row whose energy it describes
   * whatever happened to the row cadence: rows are drawn at their ring slot (the shader reads the
   * ring head, never these times), and gated rows, dropped runs and backlog-skipped frames advance
   * capture time without advancing the ring. A nominal rows-per-second would drift against that,
   * linearly with age. NaN until a row carries a time.
   */
  rowsBackAt(tS: number): number {
    return rowsBackAt((k) => this.timeAt(k), this.filledRows(), tS);
  }

  /** How many rows-back carry a capture time, as a contiguous run from the newest; cached per head. */
  private filledRows(): number {
    if (this.filledHead === this.head) return this.filledN;
    let n = 0;
    while (n < ROWS && Number.isFinite(this.timeAt(n))) n++;
    this.filledHead = this.head;
    this.filledN = n;
    return n;
  }

  /** Level (dB) of the newest row at texture fraction `u` of the full band; NaN outside. */
  levelAt(u: number): number {
    if (!(u >= 0 && u < 1)) return NaN;
    return this.latest[Math.min(this.texW - 1, Math.floor(u * this.texW))];
  }

  /** Queues one row of dB values (max-decimated to the texture width if needed) with its time. */
  push(db: Float32Array, tS = NaN) {
    const row = decimateRow(db, this.texW);
    this.autoRange(row);
    if (this.peakHold) {
      if (!this.peak) this.peak = row.slice();
      else for (let i = 0; i < row.length; i++) if (!(this.peak[i] >= row[i])) this.peak[i] = row[i];
      this.peakDirty = true;
    }
    this.pending.push({ row, mark: this.nextMark, t: tS });
    this.nextMark = 0;
    if (this.pending.length > MAX_ROWS_PER_FRAME * 4) {
      this.skipped += this.pending.length - MAX_ROWS_PER_FRAME * 4;
      this.pending.splice(0, this.pending.length - MAX_ROWS_PER_FRAME * 4);
    }
  }

  /** Floor ≈ 20th percentile of a bin sample, peak = max; smoothed, applied with 3 dB hysteresis. */
  private autoRange(row: Float32Array) {
    const s = this.scratch, step = Math.max(1, Math.floor(row.length / s.length));
    let n = 0, peak = -Infinity;
    for (let i = 0; i < row.length; i++) {
      const v = row[i];
      if (!Number.isFinite(v) || v <= UNOBSERVED_DB / 10) continue;
      if (v > peak) peak = v;
      if (i % step === 0 && n < s.length) s[n++] = v;
    }
    if (n < 8) return;
    const floor = s.subarray(0, n).sort()[Math.floor(n * 0.2)];
    const a = Number.isNaN(this.floorEst) ? 1 : 0.05;
    this.floorEst = Number.isNaN(this.floorEst) ? floor : this.floorEst + a * (floor - this.floorEst);
    this.peakEst = Number.isNaN(this.peakEst) ? peak : Math.max(peak, this.peakEst - 0.05);
    const lo = this.floorEst - 8, hi = Math.max(this.floorEst + 30, this.peakEst + 3);
    if (this.autoScale && (Math.abs(lo - this.lo) > 3 || Math.abs(hi - this.hi) > 3)) { this.lo = lo; this.hi = hi; }
  }

  private texture(fmt: number, w: number, h: number): WebGLTexture {
    const gl = this.gl, t = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texStorage2D(gl.TEXTURE_2D, 1, fmt, w, h);
    for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.NEAREST], [gl.TEXTURE_MAG_FILTER, gl.NEAREST],
      [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);
    return t;
  }

  private buildPrograms() {
    const gl = this.gl;
    const prog = (vs: string, fs: string): Prog => {
      const p = gl.createProgram()!;
      for (const [type, src] of [[gl.VERTEX_SHADER, vs], [gl.FRAGMENT_SHADER, fs]] as const) {
        const s = gl.createShader(type)!;
        gl.shaderSource(s, src);
        gl.compileShader(s);
        if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(String(gl.getShaderInfoLog(s)));
        gl.attachShader(p, s);
      }
      gl.linkProgram(p);
      if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(String(gl.getProgramInfoLog(p)));
      const u: Prog["u"] = {};
      const n = gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS) as number;
      for (let i = 0; i < n; i++) { const nm = gl.getActiveUniform(p, i)!.name; u[nm] = gl.getUniformLocation(p, nm); }
      return { p, u };
    };
    // Waterfall: ring row from head offset, max over the bins behind each pixel of the texture
    // window, marker strip.
    this.progs.wf = prog(VS_FULL, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uWf; uniform highp sampler2D uMk; uniform float uHead; uniform float uPxU;
uniform float uLo, uHi, uMkPx, uU0, uU1; in vec2 vUv; out vec4 o; ${CMAP} ${POOL}
void main(){
  ivec2 sz = textureSize(uWf,0);
  int row = int(mod(uHead - (1.0 - vUv.y) * float(sz.y), float(sz.y)));
  float k = texelFetch(uMk, ivec2(0,row), 0).r;
  if (gl_FragCoord.x < uMkPx && k > 0.0) { o = k > 0.75 ? vec4(0.89,0.63,0.03,1) : vec4(0.85,0.27,0.94,1); return; }
  float u = mix(uU0, uU1, vUv.x);
  if (u < 0.0 || u >= 1.0) { o = vec4(0,0,0,1); return; }
  ivec2 pw = poolWin(u, uPxU, sz.x); float m = -1e30;
  for (int i=0;i<64;i++){ if(i>=pw.y) break; m = max(m, texelFetch(uWf, ivec2(clamp(pw.x+i, 0, sz.x-1),row),0).r); }
  if (m > -1e25 && m < -1e19) { o = vec4(0.32,0.34,0.36,1); return; } // UNOBSERVED_DB: not observed, not quiet
  o = vec4(cmap((m-uLo)/(uHi-uLo)),1);
}`);
    // Spectrum line: vertex i at its bin centre (i + 0.5)/N, mapped through the texture window.
    this.progs.line = prog(`#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uWf; uniform int uRow; uniform int uN; uniform float uLo, uHi, uU0, uU1;
void main(){ float v = texelFetch(uWf, ivec2(gl_VertexID, uRow), 0).r;
  float u = (float(gl_VertexID) + 0.5) / float(uN);
  gl_Position = vec4((u - uU0) / (uU1 - uU0) * 2.0 - 1.0, clamp((v-uLo)/(uHi-uLo),0.0,1.0)*1.9-0.95, 0, 1); }`,
    `#version 300 es
precision mediump float; uniform vec3 uColor; out vec4 o; void main(){ o = vec4(uColor,1.0); }`);
    // DPX accumulate: H' = beta*H + hit, hit spanning this bin's and the previous bin's level.
    this.progs.acc = prog(VS_FULL, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uPrev; uniform highp sampler2D uWf; uniform int uRow; uniform float uBeta, uLo, uHi;
out vec4 o;
void main(){
  ivec2 p = ivec2(gl_FragCoord.xy); float L = float(textureSize(uPrev,0).y) - 1.0;
  float h = texelFetch(uPrev, p, 0).r * uBeta;
  float a = clamp((texelFetch(uWf, ivec2(p.x, uRow), 0).r - uLo)/(uHi-uLo), 0.0, 1.0) * L;
  float b = clamp((texelFetch(uWf, ivec2(max(p.x-1,0), uRow), 0).r - uLo)/(uHi-uLo), 0.0, 1.0) * L;
  float y = float(p.y);
  if (y >= floor(min(a,b)) && y <= ceil(max(a,b))) h += 1.0;
  o = vec4(h,0,0,1);
}`);
    this.progs.dpx = prog(VS_FULL, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uH; uniform float uPxU; uniform float uHmax, uU0, uU1; in vec2 vUv; out vec4 o; ${CMAP} ${POOL}
void main(){
  ivec2 sz = textureSize(uH,0); int y = int(clamp(vUv.y, 0.0, 0.9999) * float(sz.y));
  float u = mix(uU0, uU1, vUv.x);
  if (u < 0.0 || u >= 1.0) { o = vec4(0,0,0,1); return; }
  ivec2 pw = poolWin(u, uPxU, sz.x); float m = 0.0;
  for (int i=0;i<64;i++){ if(i>=pw.y) break; m = max(m, texelFetch(uH, ivec2(clamp(pw.x+i,0,sz.x-1), y),0).r); }
  float v = log(1.0+m)/log(1.0+uHmax);
  o = vec4(v>0.001 ? cmap(0.15+0.85*v) : vec3(0.0), 1);
}`);
  }

  private frame(now: number) {
    this.raf = requestAnimationFrame((t) => this.frame(t));
    const gl = this.gl, c = this.canvas;
    this.rafTimes.push(now);
    if (this.rafTimes.length > 120) this.rafTimes.shift();
    const n = this.rafTimes.length;
    if (n > 1) this.fps = (1000 * (n - 1)) / (this.rafTimes[n - 1] - this.rafTimes[0]);
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const cw = Math.floor(c.clientWidth * dpr), ch = Math.floor(c.clientHeight * dpr);
    if (c.width !== cw || c.height !== ch) { c.width = cw; c.height = ch; }
    // Draw into the buffer the browser actually allocated (it may clamp a huge canvas); CSS
    // stretches it over the element, so fractions of the element stay fractions of the view.
    const W = gl.drawingBufferWidth, H = gl.drawingBufferHeight;

    const tFrame = performance.now();
    let rows = this.pending;
    this.pending = [];
    if (rows.length > MAX_ROWS_PER_FRAME) { this.skipped += rows.length - MAX_ROWS_PER_FRAME; rows = rows.slice(-MAX_ROWS_PER_FRAME); }
    const mkByte = new Uint8Array(1);
    for (const r of rows) {
      this.head = (this.head + 1) % ROWS;
      gl.bindTexture(gl.TEXTURE_2D, this.wf);
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, this.head, this.texW, 1, gl.RED, gl.FLOAT, r.row);
      mkByte[0] = r.mark === MARK_GATED ? 255 : r.mark === MARK_DROP ? 128 : 0;
      gl.bindTexture(gl.TEXTURE_2D, this.mk);
      gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, this.head, 1, 1, gl.RED, gl.UNSIGNED_BYTE, mkByte);
      this.times[this.head] = r.t;
      this.latest = r.row;
      if (this.hist.length === 2) this.accumulate();
    }
    if (rows.length) this.uploadMs += 0.1 * (performance.now() - tFrame - this.uploadMs);

    const specH = Math.floor(H * SPEC_FRAC);
    if (H > 0) this.specFrac = specH / H;
    const pxU = Math.max(1e-12, this.u1 - this.u0) / Math.max(1, W); // band fraction per screen pixel
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, W, H);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);

    const wf = this.progs.wf;
    gl.viewport(0, 0, W, H - specH);
    gl.useProgram(wf.p);
    this.bind(0, this.wf, wf.u.uWf);
    this.bind(1, this.mk, wf.u.uMk);
    gl.uniform1f(wf.u.uHead, this.head + 1);
    gl.uniform1f(wf.u.uPxU, pxU);
    gl.uniform1f(wf.u.uLo, this.lo);
    gl.uniform1f(wf.u.uHi, this.hi);
    gl.uniform1f(wf.u.uMkPx, 6 * dpr);
    gl.uniform1f(wf.u.uU0, this.u0);
    gl.uniform1f(wf.u.uU1, this.u1);
    gl.drawArrays(gl.TRIANGLES, 0, 3);

    gl.viewport(0, H - specH, W, specH);
    if (this.hist.length === 2) {
      const d = this.progs.dpx;
      gl.useProgram(d.p);
      this.bind(0, this.hist[0].tex, d.u.uH);
      gl.uniform1f(d.u.uPxU, pxU);
      gl.uniform1f(d.u.uHmax, 1 / (1 - this.beta));
      gl.uniform1f(d.u.uU0, this.u0);
      gl.uniform1f(d.u.uU1, this.u1);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    }
    const l = this.progs.line;
    gl.useProgram(l.p);
    this.bind(0, this.wf, l.u.uWf);
    gl.uniform1i(l.u.uRow, this.head);
    gl.uniform1i(l.u.uN, this.texW);
    gl.uniform1f(l.u.uLo, this.lo);
    gl.uniform1f(l.u.uHi, this.hi);
    gl.uniform1f(l.u.uU0, this.u0);
    gl.uniform1f(l.u.uU1, this.u1);
    gl.uniform3f(l.u.uColor, ...this.lineColor);
    gl.drawArrays(gl.LINE_STRIP, 0, this.texW);
    if (this.peakHold && this.peak) {
      if (this.peakDirty) {
        gl.bindTexture(gl.TEXTURE_2D, this.peakTex);
        gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, this.texW, 1, gl.RED, gl.FLOAT, this.peak);
        this.peakDirty = false;
      }
      this.bind(0, this.peakTex, l.u.uWf);
      gl.uniform1i(l.u.uRow, 0);
      gl.uniform3f(l.u.uColor, 1.0, 0.35, 0.35);
      gl.drawArrays(gl.LINE_STRIP, 0, this.texW);
    }
    this.frameMs += 0.1 * (performance.now() - tFrame - this.frameMs);
  }

  private accumulate() {
    const gl = this.gl, a = this.progs.acc, [src, dst] = this.hist;
    gl.bindFramebuffer(gl.FRAMEBUFFER, dst.fb);
    gl.viewport(0, 0, this.texW, LEVELS);
    gl.useProgram(a.p);
    this.bind(0, src.tex, a.u.uPrev);
    this.bind(1, this.wf, a.u.uWf);
    gl.uniform1i(a.u.uRow, this.head);
    gl.uniform1f(a.u.uBeta, this.beta);
    gl.uniform1f(a.u.uLo, this.lo);
    gl.uniform1f(a.u.uHi, this.hi);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    this.hist = [dst, src];
  }

  private bind(unit: number, tex: WebGLTexture, loc: WebGLUniformLocation | null) {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0 + unit);
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.uniform1i(loc, unit);
  }
}
