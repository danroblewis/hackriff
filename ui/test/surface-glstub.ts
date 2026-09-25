// A recording WebGL2 stub, enough to run `ui/src/surface/surface.ts` unchanged (the pattern
// ui/test/timebox.test.ts established for `Waterfall`).
//
// Programs learn their uniform names by reading the shader sources they were given, uniform writes
// are remembered per location, and every draw records the program's uniforms as they stood at that
// instant — so a test asserts what the render pass actually submitted, not what a helper called in
// the style the renderer is hoped to use. Clears are recorded too, because the colour a pane's
// ground is cleared to is itself a claim (T-437 F3).

export interface GlOp {
  kind: "viewport" | "scissor" | "clear" | "draw";
  args: number[];
  u?: Record<string, number[]>;
  /** For a draw: the texture bound to each sampler unit at that instant, so a test can rasterise
   * the frame from the bytes that were really uploaded rather than from the ones it hoped were
   * (T-441 — "assert on what is drawn, not on a flag"). */
  units?: Record<number, Upload | undefined>;
}

/** One uploaded texture: its format, size and the bytes `texSubImage2D` was handed. */
export interface Upload { fmt: number; w: number; h: number; data: ArrayLike<number> }

interface Prog { id: number; shaders: string[]; uniforms: string[] }

export function stubGl(w = 800, h = 600) {
  const K: Record<string, number> = {
    VERTEX_SHADER: 1, FRAGMENT_SHADER: 2, COMPILE_STATUS: 3, LINK_STATUS: 4, ACTIVE_UNIFORMS: 5,
    TEXTURE_2D: 7, RED: 8, FLOAT: 9, UNSIGNED_BYTE: 10, TRIANGLES: 11, TRIANGLE_STRIP: 13,
    TEXTURE0: 100, TEXTURE1: 101, COLOR_BUFFER_BIT: 16384, R8: 20, R16F: 21,
  };
  let nextK = 200;
  const ops: GlOp[] = [];
  const shaders: string[] = [];
  const textures: object[] = [];
  const deleted: object[] = [];
  const uploads: Upload[] = [];
  const texData = new Map<object, Upload>();
  const bound = new Map<number, object>();
  const values = new Map<object, number[]>();
  const locs = new Map<string, object>();
  let current: Prog | null = null;
  let nextProg = 0;
  let clearColor = [0, 0, 0, 1];
  let lastTex: object | null = null;
  let unit = 0;

  const snapshot = (p: Prog | null): Record<string, number[]> => {
    const out: Record<string, number[]> = {};
    if (p) for (const n of p.uniforms) { const v = values.get(locs.get(`${p.id}/${n}`)!); if (v) out[n] = v; }
    return out;
  };
  const setU = (loc: unknown, ...v: number[]) => { if (loc) values.set(loc as object, v); };

  const base: Record<string, unknown> = {
    drawingBufferWidth: w, drawingBufferHeight: h,
    getParameter: () => 4096,
    getExtension: () => null,
    createTexture: () => { const t = { id: textures.length }; textures.push(t); return t; },
    deleteTexture: (t: object) => { deleted.push(t); },
    bindTexture: (_target: number, t: object) => { lastTex = t; bound.set(unit, t); },
    texStorage2D: (_t: number, _l: number, fmt: number, tw: number, th: number) => {
      const up: Upload = { fmt, w: tw, h: th, data: [] };
      uploads.push(up);
      if (lastTex) texData.set(lastTex, up);
    },
    texSubImage2D: (..._a: unknown[]) => {
      const data = _a[_a.length - 1] as ArrayLike<number>;
      const y = Number(_a[3] ?? 0), rows = Number(_a[5] ?? 0);
      const up = lastTex ? texData.get(lastTex) : undefined;
      // A row patch (T-893) rewrites rows [y, y + rows) of the texture bound NOW, not the newest one.
      if (up && !(y === 0 && rows === up.h)) {
        const full = Array.from(up.data.length ? up.data : new Array<number>(up.w * up.h).fill(0));
        for (let i = 0; i < data.length; i++) full[y * up.w + i] = data[i];
        up.data = full;
        return;
      }
      if (up) { up.data = data; return; }
      if (uploads.length) uploads[uploads.length - 1].data = data;
    },
    texParameteri: () => undefined,
    pixelStorei: () => undefined,
    createVertexArray: () => ({}),
    bindVertexArray: () => undefined,
    createProgram: (): Prog => ({ id: nextProg++, shaders: [], uniforms: [] }),
    createShader: () => ({ src: "" }),
    shaderSource: (s: { src: string }, src: string) => { s.src = src; shaders.push(src); },
    attachShader: (p: Prog, s: { src: string }) => { p.shaders.push(s.src); },
    compileShader: () => undefined,
    getShaderParameter: () => true,
    getShaderInfoLog: () => "",
    getProgramInfoLog: () => "",
    linkProgram: (p: Prog) => {
      const names = new Set<string>();
      for (const src of p.shaders) {
        for (const m of src.matchAll(/\buniform\s+\w+\s+([^;]+);/g)) {
          for (const part of m[1].split(",")) names.add(part.trim().split(/[\s[]/)[0]);
        }
      }
      p.uniforms = [...names];
    },
    getProgramParameter: (p: Prog, k: number) => (k === K.ACTIVE_UNIFORMS ? p.uniforms.length : true),
    getActiveUniform: (p: Prog, i: number) => ({ name: p.uniforms[i] }),
    getUniformLocation: (p: Prog, name: string) => {
      const key = `${p.id}/${name}`;
      if (!locs.has(key)) locs.set(key, { key });
      return locs.get(key)!;
    },
    useProgram: (p: Prog) => { current = p; },
    uniform1i: setU, uniform1f: setU, uniform2f: setU, uniform3f: setU, uniform4f: setU,
    enable: () => undefined, disable: () => undefined,
    activeTexture: (u: number) => { unit = u - K.TEXTURE0; },
    viewport: (...a: number[]) => { ops.push({ kind: "viewport", args: a }); },
    scissor: (...a: number[]) => { ops.push({ kind: "scissor", args: a }); },
    clearColor: (...a: number[]) => { clearColor = a; },
    clear: () => { ops.push({ kind: "clear", args: clearColor.slice() }); },
    drawArrays: (mode: number, first: number, count: number) => {
      const units: Record<number, Upload | undefined> = {};
      for (const [u, t] of bound) units[u] = texData.get(t);
      ops.push({ kind: "draw", args: [mode, first, count], u: snapshot(current), units });
    },
  };
  const gl = new Proxy(base, {
    get(t, k) {
      if (k in t) return t[k as string];
      const n = String(k);
      if (/^[A-Z][A-Z0-9_]*$/.test(n)) return (K[n] ??= nextK++);
      return () => undefined;
    },
  });
  let contexts = 0;
  const canvas = {
    width: w, height: h, clientWidth: w, clientHeight: h,
    getContext: () => { contexts++; return gl; },
  } as unknown as HTMLCanvasElement;
  return {
    gl, ops, canvas, shaders, uploads, textures, deleted,
    width: w, height: h,
    contextCount: () => contexts,
    draws: () => ops.filter((o) => o.kind === "draw"),
    clears: () => ops.filter((o) => o.kind === "clear"),
    reset: () => { ops.length = 0; },
  };
}
