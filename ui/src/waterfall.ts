// WebGL2 waterfall + spectrum line + GPU DPX persistence, ported from spike S3
// (spikes/s3-web-waterfall/client/src/main.ts, measured at 60 fps up to 16384 bins).
// Differences: rows arrive as f32 dB (hk-stream `rf32_le` spectrum), stored in an R32F texture
// ring and mapped to colour in the shader, so auto-ranging re-maps history for free; a left-edge
// strip marks dropped/gated runs.

const ROWS = 512;
const LEVELS = 256;
const TAU_S = 0.5;
const MAX_ROWS_PER_FRAME = 16;
export const MARK_DROP = 1;
export const MARK_GATED = 2;

const VS_FULL = `#version 300 es
out vec2 vUv;
void main(){ vec2 p = vec2((gl_VertexID<<1)&2, gl_VertexID&2); vUv = p; gl_Position = vec4(p*2.0-1.0,0,1); }`;
const CMAP = `
vec3 cmap(float x){ x = clamp(x,0.0,1.0);
  vec3 c0=vec3(0.0,0.0,0.04), c1=vec3(0.05,0.1,0.55), c2=vec3(0.0,0.7,0.9), c3=vec3(0.95,0.9,0.1), c4=vec3(0.95,0.2,0.05), c5=vec3(1.0);
  if(x<0.2) return mix(c0,c1,x/0.2); if(x<0.45) return mix(c1,c2,(x-0.2)/0.25);
  if(x<0.7) return mix(c2,c3,(x-0.45)/0.25); if(x<0.9) return mix(c3,c4,(x-0.7)/0.2); return mix(c4,c5,(x-0.9)/0.1); }`;

type Prog = { p: WebGLProgram; u: Record<string, WebGLUniformLocation | null> };

export class Waterfall {
  readonly bins: number;
  lo = -120;
  hi = -60;
  fps = 0;
  skipped = 0;
  latest: Float32Array;
  private gl: WebGL2RenderingContext;
  private texW: number;
  private wf: WebGLTexture;
  private mk: WebGLTexture;
  private hist: { tex: WebGLTexture; fb: WebGLFramebuffer }[] = [];
  private progs: Record<string, Prog> = {};
  private head = 0;
  private pending: { row: Float32Array; mark: number }[] = [];
  private nextMark = 0;
  private beta: number;
  private floorEst = NaN;
  private peakEst = NaN;
  private rafTimes: number[] = [];
  private raf = 0;
  private scratch: Float32Array;

  constructor(private canvas: HTMLCanvasElement, bins: number, rowRateHz: number) {
    const gl = canvas.getContext("webgl2", { antialias: false, alpha: false });
    if (!gl) throw new Error("WebGL2 unavailable");
    this.gl = gl;
    this.bins = bins;
    this.texW = Math.min(bins, gl.getParameter(gl.MAX_TEXTURE_SIZE) as number);
    this.latest = new Float32Array(this.texW);
    this.scratch = new Float32Array(Math.min(this.texW, 512));
    this.beta = Math.exp(-1 / (TAU_S * Math.max(1, rowRateHz)));
    this.buildPrograms();
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    this.wf = this.texture(gl.R32F, this.texW, ROWS);
    // Rows not yet received read as "far below range" (dark), not 0 dB (white).
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, this.texW, ROWS, gl.RED, gl.FLOAT, new Float32Array(this.texW * ROWS).fill(-1e30));
    this.mk = this.texture(gl.R8, 1, ROWS);
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

  /** Marks the next row (a dropped or gated run precedes it). */
  mark(kind: number) {
    this.nextMark = Math.max(this.nextMark, kind);
  }

  /** Queues one row of dB values (max-decimated to the texture width if needed). */
  push(db: Float32Array) {
    let row: Float32Array;
    if (db.length === this.texW) row = db.slice();
    else {
      row = new Float32Array(this.texW);
      const r = db.length / this.texW;
      for (let i = 0; i < this.texW; i++) {
        let m = -Infinity;
        const e = Math.min(db.length, Math.floor((i + 1) * r));
        for (let j = Math.floor(i * r); j < e; j++) if (db[j] > m) m = db[j];
        row[i] = m;
      }
    }
    this.autoRange(row);
    this.pending.push({ row, mark: this.nextMark });
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
      if (!Number.isFinite(v)) continue;
      if (v > peak) peak = v;
      if (i % step === 0 && n < s.length) s[n++] = v;
    }
    if (n < 8) return;
    const floor = s.subarray(0, n).sort()[Math.floor(n * 0.2)];
    const a = Number.isNaN(this.floorEst) ? 1 : 0.05;
    this.floorEst = Number.isNaN(this.floorEst) ? floor : this.floorEst + a * (floor - this.floorEst);
    this.peakEst = Number.isNaN(this.peakEst) ? peak : Math.max(peak, this.peakEst - 0.05);
    const lo = this.floorEst - 8, hi = Math.max(this.floorEst + 30, this.peakEst + 3);
    if (Math.abs(lo - this.lo) > 3 || Math.abs(hi - this.hi) > 3) { this.lo = lo; this.hi = hi; }
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
    // Waterfall: ring row from head offset, max over the bins behind each pixel, marker strip.
    this.progs.wf = prog(VS_FULL, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uWf; uniform highp sampler2D uMk; uniform float uHead; uniform int uStep;
uniform float uLo, uHi, uMkPx; in vec2 vUv; out vec4 o; ${CMAP}
void main(){
  ivec2 sz = textureSize(uWf,0);
  int row = int(mod(uHead - (1.0 - vUv.y) * float(sz.y), float(sz.y)));
  float k = texelFetch(uMk, ivec2(0,row), 0).r;
  if (gl_FragCoord.x < uMkPx && k > 0.0) { o = k > 0.75 ? vec4(0.89,0.63,0.03,1) : vec4(0.85,0.27,0.94,1); return; }
  int x0 = int(vUv.x * float(sz.x)); float m = -1e30;
  for (int i=0;i<64;i++){ if(i>=uStep) break; m = max(m, texelFetch(uWf, ivec2(min(x0+i, sz.x-1),row),0).r); }
  o = vec4(cmap((m-uLo)/(uHi-uLo)),1);
}`);
    this.progs.line = prog(`#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uWf; uniform int uRow; uniform int uN; uniform float uLo, uHi;
void main(){ float v = texelFetch(uWf, ivec2(gl_VertexID, uRow), 0).r;
  gl_Position = vec4(float(gl_VertexID)/float(uN-1)*2.0-1.0, clamp((v-uLo)/(uHi-uLo),0.0,1.0)*1.9-0.95, 0, 1); }`,
    `#version 300 es
precision mediump float; out vec4 o; void main(){ o = vec4(1.0,1.0,0.6,1.0); }`);
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
uniform highp sampler2D uH; uniform int uStep; uniform float uHmax; in vec2 vUv; out vec4 o; ${CMAP}
void main(){
  ivec2 sz = textureSize(uH,0); int y = int(clamp(vUv.y, 0.0, 0.9999) * float(sz.y));
  int x0 = int(vUv.x * float(sz.x)); float m = 0.0;
  for (int i=0;i<64;i++){ if(i>=uStep) break; m = max(m, texelFetch(uH, ivec2(min(x0+i,sz.x-1), y),0).r); }
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
    const W = Math.floor(c.clientWidth * dpr), H = Math.floor(c.clientHeight * dpr);
    if (c.width !== W || c.height !== H) { c.width = W; c.height = H; }

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
      this.latest = r.row;
      if (this.hist.length === 2) this.accumulate();
    }

    const specH = Math.floor(H * 0.35), step = Math.max(1, Math.min(64, Math.ceil(this.texW / Math.max(1, W))));
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
    gl.uniform1i(wf.u.uStep, step);
    gl.uniform1f(wf.u.uLo, this.lo);
    gl.uniform1f(wf.u.uHi, this.hi);
    gl.uniform1f(wf.u.uMkPx, 6 * dpr);
    gl.drawArrays(gl.TRIANGLES, 0, 3);

    gl.viewport(0, H - specH, W, specH);
    if (this.hist.length === 2) {
      const d = this.progs.dpx;
      gl.useProgram(d.p);
      this.bind(0, this.hist[0].tex, d.u.uH);
      gl.uniform1i(d.u.uStep, step);
      gl.uniform1f(d.u.uHmax, 1 / (1 - this.beta));
      gl.drawArrays(gl.TRIANGLES, 0, 3);
    }
    const l = this.progs.line;
    gl.useProgram(l.p);
    this.bind(0, this.wf, l.u.uWf);
    gl.uniform1i(l.u.uRow, this.head);
    gl.uniform1i(l.u.uN, this.texW);
    gl.uniform1f(l.u.uLo, this.lo);
    gl.uniform1f(l.u.uHi, this.hi);
    gl.drawArrays(gl.LINE_STRIP, 0, this.texW);
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
