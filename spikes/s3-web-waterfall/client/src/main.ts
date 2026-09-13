// Spike S3 client: WebGL2 waterfall (texture ring), live spectrum line, DPX persistence.
// URL params: bins, fps, dtype (u8|f32), persist (gpu|cpu|off), levels (256), rows (512),
//             tau (persistence decay seconds, 0.5), ws (override ws url), finish (1 = gl.finish per rAF)
const q = new URLSearchParams(location.search);
const BINS = +(q.get("bins") ?? 4096);
const FPS = +(q.get("fps") ?? 30);
const DTYPE = q.get("dtype") ?? "u8";
const PERSIST = q.get("persist") ?? "gpu";
const LEVELS = +(q.get("levels") ?? 256);
const ROWS = +(q.get("rows") ?? 512);
const TAU = +(q.get("tau") ?? 0.5);
const FINISH = q.get("finish") === "1";
const SYNC_READ = q.get("finish") === "2";
const px1 = new Uint8Array(4);
const MAX_ROWS_PER_RAF = 16;

const canvas = document.getElementById("c") as HTMLCanvasElement;
const statsEl = document.getElementById("stats")!;
const gl = canvas.getContext("webgl2", { antialias: false, alpha: false, preserveDrawingBuffer: false, powerPreference: "high-performance" })!;
if (!gl) { statsEl.textContent = "WebGL2 unavailable"; throw new Error("no webgl2"); }

const dbg = gl.getExtension("WEBGL_debug_renderer_info");
const renderer = dbg ? String(gl.getParameter(dbg.UNMASKED_RENDERER_WEBGL)) : String(gl.getParameter(gl.RENDERER));
const maxTex = gl.getParameter(gl.MAX_TEXTURE_SIZE) as number;
const floatRT = !!gl.getExtension("EXT_color_buffer_float");
const halfRT = floatRT || !!gl.getExtension("EXT_color_buffer_half_float");
const TEXW = Math.min(BINS, maxTex);          // bins > MAX_TEXTURE_SIZE: CPU max-decimate on ingest

// ---------------- shaders ----------------
function sh(type: number, src: string) {
  const s = gl.createShader(type)!; gl.shaderSource(s, src); gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s) + "\n" + src);
  return s;
}
function prog(vs: string, fs: string) {
  const p = gl.createProgram()!;
  gl.attachShader(p, sh(gl.VERTEX_SHADER, vs)); gl.attachShader(p, sh(gl.FRAGMENT_SHADER, fs));
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p)!);
  const u: Record<string, WebGLUniformLocation | null> = {};
  const n = gl.getProgramParameter(p, gl.ACTIVE_UNIFORMS) as number;
  for (let i = 0; i < n; i++) { const nm = gl.getActiveUniform(p, i)!.name; u[nm] = gl.getUniformLocation(p, nm); }
  return { p, u };
}
// full-screen triangle, no attributes
const VS_FS = `#version 300 es
out vec2 vUv;
void main(){ vec2 p = vec2((gl_VertexID<<1)&2, gl_VertexID&2); vUv = p; gl_Position = vec4(p*2.0-1.0,0,1); }`;
const CMAP = `
vec3 cmap(float x){ x = clamp(x,0.0,1.0);
  vec3 c0=vec3(0.0,0.0,0.04), c1=vec3(0.05,0.1,0.55), c2=vec3(0.0,0.7,0.9), c3=vec3(0.95,0.9,0.1), c4=vec3(0.95,0.2,0.05), c5=vec3(1.0,1.0,1.0);
  if(x<0.2) return mix(c0,c1,x/0.2); if(x<0.45) return mix(c1,c2,(x-0.2)/0.25);
  if(x<0.7) return mix(c2,c3,(x-0.45)/0.25); if(x<0.9) return mix(c3,c4,(x-0.7)/0.2); return mix(c4,c5,(x-0.9)/0.1); }`;

// Waterfall display: texture ring, UV offset scroll, max-decimation across bins per pixel, colour map in shader.
const wfProg = prog(VS_FS, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uWf; uniform float uHead; uniform int uStep; uniform vec2 uView; uniform float uLo, uHi;
in vec2 vUv; out vec4 o;
${CMAP}
void main(){
  ivec2 sz = textureSize(uWf,0);
  // newest row at top; scroll = offset of the ring head
  float r = mod(uHead - (1.0 - vUv.y) * float(sz.y), float(sz.y));
  int row = int(r);
  int x0 = int(vUv.x * float(sz.x));
  float m = 0.0;
  for (int k=0;k<64;k++){ if(k>=uStep) break; int xx=min(x0+k, sz.x-1); m = max(m, texelFetch(uWf, ivec2(xx,row),0).r); }
  o = vec4(cmap((m-uLo)/(uHi-uLo)),1);
}`);

// Spectrum line: vertices read the newest waterfall row by gl_VertexID.
const lineProg = prog(`#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uWf; uniform int uRow; uniform int uN; uniform float uLo, uHi;
void main(){ float v = texelFetch(uWf, ivec2(gl_VertexID, uRow), 0).r;
  gl_Position = vec4(float(gl_VertexID)/float(uN-1)*2.0-1.0, clamp((v-uLo)/(uHi-uLo),0.0,1.0)*1.9-0.95, 0, 1); }`,
`#version 300 es
precision mediump float; out vec4 o; void main(){ o = vec4(1.0,1.0,0.6,1.0); }`);

// DPX accumulate (GPU): H' = beta*H + hit, hit if level y lies between this bin and the previous bin (connected trace).
const accProg = prog(VS_FS, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uPrev; uniform highp sampler2D uWf; uniform int uRow; uniform float uBeta; uniform float uLevels;
out vec4 o;
void main(){
  ivec2 p = ivec2(gl_FragCoord.xy);
  float h = texelFetch(uPrev, p, 0).r * uBeta;
  float a = texelFetch(uWf, ivec2(p.x, uRow), 0).r * (uLevels-1.0);
  float b = texelFetch(uWf, ivec2(max(p.x-1,0), uRow), 0).r * (uLevels-1.0);
  float y = float(p.y);
  if (y >= floor(min(a,b)) && y <= ceil(max(a,b))) h += 1.0;
  o = vec4(h,0,0,1);
}`);

// DPX display: max over bins per pixel, log intensity.
const dpxProg = prog(VS_FS, `#version 300 es
precision highp float; precision highp int;
uniform highp sampler2D uH; uniform int uStep; uniform float uHmax; uniform float uLo, uHi;
in vec2 vUv; out vec4 o;
${CMAP}
void main(){
  ivec2 sz = textureSize(uH,0);
  float lv = mix(uLo, uHi, vUv.y);                  // displayed level range
  int y = int(clamp(lv, 0.0, 0.9999) * float(sz.y));
  int x0 = int(vUv.x * float(sz.x));
  float m = 0.0;
  for (int k=0;k<64;k++){ if(k>=uStep) break; m = max(m, texelFetch(uH, ivec2(min(x0+k,sz.x-1), y),0).r); }
  float v = log(1.0+m)/log(1.0+uHmax);
  o = vec4(v>0.001 ? cmap(0.15+0.85*v) : vec3(0.0), 1);
}`);

// ---------------- textures ----------------
gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
const wfTex = gl.createTexture()!;
gl.bindTexture(gl.TEXTURE_2D, wfTex);
gl.texStorage2D(gl.TEXTURE_2D, 1, gl.R8, TEXW, ROWS);
for (const [k, v] of [[gl.TEXTURE_MIN_FILTER, gl.NEAREST], [gl.TEXTURE_MAG_FILTER, gl.NEAREST], [gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE], [gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE]]) gl.texParameteri(gl.TEXTURE_2D, k, v);

function mkHist(): { tex: WebGLTexture; fb: WebGLFramebuffer | null } {
  const tex = gl.createTexture()!;
  gl.bindTexture(gl.TEXTURE_2D, tex);
  const fmt = PERSIST === "cpu" ? gl.R32F : floatRT ? gl.R16F : halfRT ? gl.R16F : gl.RGBA8;
  gl.texStorage2D(gl.TEXTURE_2D, 1, fmt, TEXW, LEVELS);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST); gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  let fb: WebGLFramebuffer | null = null;
  if (PERSIST === "gpu") {
    fb = gl.createFramebuffer()!;
    gl.bindFramebuffer(gl.FRAMEBUFFER, fb);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, tex, 0);
    gl.clearColor(0, 0, 0, 0); gl.clear(gl.COLOR_BUFFER_BIT);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  }
  return { tex, fb };
}
let histA = mkHist(), histB = PERSIST === "gpu" ? mkHist() : histA;
const cpuHist = PERSIST === "cpu" ? new Float32Array(TEXW * LEVELS) : null;
const beta = Math.exp(-1 / (TAU * FPS));
const hmax = 1 / (1 - beta);

// ---------------- stats ----------------
const S = {
  renderer, maxTex, texW: TEXW, floatRT, config: { BINS, FPS, DTYPE, PERSIST, LEVELS, ROWS, TAU, FINISH },
  header: null as any,
  rx: 0, rxBytes: 0, gaps: 0, lastSeq: -1, clientDropped: 0, rendered: 0,
  rafTimes: [] as number[],      // rAF timestamps (ms, performance.now)
  workMs: [] as number[],        // JS time spent inside rAF callback (incl. gl.finish if enabled)
  latTsMs: [] as number[],       // frame t_unix_ms -> rAF (wall clock; same-host only meaningful)
  latRxMs: [] as number[],       // ws receive -> rAF
  ingestMs: [] as number[],      // per-message decode/quantise time
};
(window as any).__s3 = S;
(window as any).__s3reset = () => { S.rx = 0; S.rxBytes = 0; S.gaps = 0; S.clientDropped = 0; S.rendered = 0; S.rafTimes = []; S.workMs = []; S.latTsMs = []; S.latRxMs = []; S.ingestMs = []; };

// ---------------- ingest ----------------
type Row = { seq: number; t: number; rx: number; data: Uint8Array };
let pending: Row[] = [];
const scratch = new Float32Array(BINS);
function ingest(buf: ArrayBuffer) {
  const t0 = performance.now();
  const dv = new DataView(buf);
  const seq = dv.getUint32(0, true), dtype = dv.getUint8(4), bins = dv.getUint32(8, true), t = dv.getFloat64(16, true);
  if (S.lastSeq >= 0 && seq !== S.lastSeq + 1) S.gaps += (seq - S.lastSeq - 1) >>> 0;
  S.lastSeq = seq; S.rx++; S.rxBytes += buf.byteLength;
  const out = new Uint8Array(TEXW);
  const h = S.header;
  if (dtype === 1) {
    const src = new Uint8Array(buf, 24, bins);
    if (bins === TEXW) out.set(src);
    else { const r = bins / TEXW; for (let i = 0; i < TEXW; i++) { let m = 0; const e = Math.min(bins, Math.floor((i + 1) * r)); for (let j = Math.floor(i * r); j < e; j++) if (src[j] > m) m = src[j]; out[i] = m; } }
  } else {
    // f32 dBFS -> u8 over the header's display range (the realistic core output; CPU quantise cost)
    const src = new Float32Array(buf, 24, bins);
    const lo = h.u8_min_db, k = 255 / (h.u8_max_db - h.u8_min_db), r = bins / TEXW;
    if (bins === TEXW) for (let i = 0; i < bins; i++) { const v = (src[i] - lo) * k; out[i] = v < 0 ? 0 : v > 255 ? 255 : v; }
    else for (let i = 0; i < TEXW; i++) { let m = -1e9; const e = Math.min(bins, Math.floor((i + 1) * r)); for (let j = Math.floor(i * r); j < e; j++) if (src[j] > m) m = src[j]; const v = (m - lo) * k; out[i] = v < 0 ? 0 : v > 255 ? 255 : v; }
  }
  void scratch;
  pending.push({ seq, t, rx: performance.timeOrigin + t0, data: out });
  if (pending.length > MAX_ROWS_PER_RAF * 4) { S.clientDropped += pending.length - MAX_ROWS_PER_RAF * 4; pending = pending.slice(-MAX_ROWS_PER_RAF * 4); }
  S.ingestMs.push(performance.now() - t0);
}

function connect() {
  const wsUrl = q.get("ws") ?? `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws?bins=${BINS}&fps=${FPS}&dtype=${DTYPE}`;
  const ws = new WebSocket(wsUrl);
  ws.binaryType = "arraybuffer";
  ws.onmessage = (ev) => {
    if (typeof ev.data === "string") { S.header = JSON.parse(ev.data); return; }
    ingest(ev.data as ArrayBuffer);
  };
  ws.onclose = () => { statsEl.textContent = "disconnected; retrying"; setTimeout(connect, 1000); };
}
connect();

// ---------------- render ----------------
let head = 0; // index of the newest row written
const vao = gl.createVertexArray(); gl.bindVertexArray(vao);

function resize() {
  const dpr = Math.min(window.devicePixelRatio || 1, 2);
  const w = Math.floor(canvas.clientWidth * dpr), h = Math.floor(canvas.clientHeight * dpr);
  if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; }
}

function accumulateRow(row: number, data: Uint8Array) {
  if (PERSIST === "gpu") {
    gl.bindFramebuffer(gl.FRAMEBUFFER, histB.fb);
    gl.viewport(0, 0, TEXW, LEVELS);
    gl.useProgram(accProg.p);
    gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, histA.tex); gl.uniform1i(accProg.u.uPrev, 0);
    gl.activeTexture(gl.TEXTURE1); gl.bindTexture(gl.TEXTURE_2D, wfTex); gl.uniform1i(accProg.u.uWf, 1);
    gl.uniform1i(accProg.u.uRow, row); gl.uniform1f(accProg.u.uBeta, beta); gl.uniform1f(accProg.u.uLevels, LEVELS);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
    const t = histA; histA = histB; histB = t;
  } else if (PERSIST === "cpu" && cpuHist) {
    const H = cpuHist, L = LEVELS, sc = (L - 1) / 255;
    for (let i = 0; i < H.length; i++) H[i] *= beta;
    let prev = data[0] * sc;
    for (let x = 0; x < TEXW; x++) {
      const a = data[x] * sc; const lo = Math.floor(Math.min(a, prev)), hi = Math.ceil(Math.max(a, prev));
      for (let y = lo; y <= hi; y++) H[y * TEXW + x] += 1;
      prev = a;
    }
  }
}

let lastUi = 0;
function frame(now: number) {
  requestAnimationFrame(frame);
  const t0 = performance.now();
  S.rafTimes.push(now);
  resize();
  let rows = pending; pending = [];
  if (rows.length > MAX_ROWS_PER_RAF) { S.clientDropped += rows.length - MAX_ROWS_PER_RAF; rows = rows.slice(-MAX_ROWS_PER_RAF); }
  const wall = performance.timeOrigin + now;
  for (const r of rows) {
    head = (head + 1) % ROWS;
    gl.bindTexture(gl.TEXTURE_2D, wfTex);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, head, TEXW, 1, gl.RED, gl.UNSIGNED_BYTE, r.data);
    accumulateRow(head, r.data);
    S.rendered++; S.latTsMs.push(wall - r.t); S.latRxMs.push(wall - r.rx);
  }
  if (PERSIST === "cpu" && cpuHist && rows.length) {
    gl.bindTexture(gl.TEXTURE_2D, histA.tex);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, TEXW, LEVELS, gl.RED, gl.FLOAT, cpuHist);
  }

  const W = canvas.width, H = canvas.height, specH = Math.floor(H * 0.35);
  const step = Math.max(1, Math.min(64, Math.ceil(TEXW / W)));
  const lo = 20 / 255, hi = 200 / 255; // display range in u8-normalised dB (~-120.6 .. -35.9 dBFS)
  gl.bindFramebuffer(gl.FRAMEBUFFER, null);
  gl.viewport(0, 0, W, H); gl.clearColor(0, 0, 0, 1); gl.clear(gl.COLOR_BUFFER_BIT);

  // waterfall (bottom)
  gl.viewport(0, 0, W, H - specH);
  gl.useProgram(wfProg.p);
  gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, wfTex); gl.uniform1i(wfProg.u.uWf, 0);
  gl.uniform1f(wfProg.u.uHead, head + 1); gl.uniform1i(wfProg.u.uStep, step); gl.uniform1f(wfProg.u.uLo, lo); gl.uniform1f(wfProg.u.uHi, hi);
  gl.drawArrays(gl.TRIANGLES, 0, 3);

  // persistence + spectrum line (top)
  gl.viewport(0, H - specH, W, specH);
  if (PERSIST !== "off") {
    gl.useProgram(dpxProg.p);
    gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, histA.tex); gl.uniform1i(dpxProg.u.uH, 0);
    gl.uniform1i(dpxProg.u.uStep, step); gl.uniform1f(dpxProg.u.uHmax, hmax); gl.uniform1f(dpxProg.u.uLo, lo); gl.uniform1f(dpxProg.u.uHi, hi);
    gl.drawArrays(gl.TRIANGLES, 0, 3);
  }
  gl.useProgram(lineProg.p);
  gl.activeTexture(gl.TEXTURE0); gl.bindTexture(gl.TEXTURE_2D, wfTex); gl.uniform1i(lineProg.u.uWf, 0);
  gl.uniform1i(lineProg.u.uRow, head); gl.uniform1i(lineProg.u.uN, TEXW); gl.uniform1f(lineProg.u.uLo, lo); gl.uniform1f(lineProg.u.uHi, hi);
  gl.drawArrays(gl.LINE_STRIP, 0, TEXW);
  // finish=1: gl.finish() (Chrome's command buffer makes this ~non-blocking);
  // finish=2: 1-pixel readPixels, which blocks until the GPU has executed this frame -> workMs ≈ CPU+GPU time
  if (FINISH) gl.finish();
  if (SYNC_READ) gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px1);
  S.workMs.push(performance.now() - t0);

  if (now - lastUi > 500) {
    lastUi = now;
    const n = S.rafTimes.length, k = Math.min(n, 120);
    const fps = k > 1 ? (1000 * (k - 1)) / (S.rafTimes[n - 1] - S.rafTimes[n - k]) : 0;
    const lat = S.latTsMs.slice(-60).sort((a, b) => a - b);
    statsEl.textContent =
      `${renderer}\nbins=${BINS} (tex ${TEXW}) in=${FPS}fps ${DTYPE} persist=${PERSIST}${floatRT ? "" : " (no float RT)"}\n` +
      `rAF ${fps.toFixed(1)} fps  rx ${S.rx}  rendered ${S.rendered}  seq-gaps ${S.gaps}  client-drop ${S.clientDropped}\n` +
      `latency ts→rAF p50 ${(lat[lat.length >> 1] ?? 0).toFixed(1)} ms  rx ${(S.rxBytes / 1e6).toFixed(1)} MB`;
    // bound memory for long runs (bench script samples before trimming)
    if (S.rafTimes.length > 200000) (window as any).__s3reset();
  }
}
requestAnimationFrame(frame);
