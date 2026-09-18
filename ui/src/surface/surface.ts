// The renderer core of the unified surface: **one canvas element, one WebGL2 context, N scissored
// panes, one shared tile LRU** (T-440, docs/16 §8.3).
//
// Why one context. Textures do not cross WebGL contexts, so a context per pane is a cache per pane,
// and a tile visible in eight panes would upload eight times — T-437 measured 18.68 MB shared
// against 149.44 MB unshared, on the same screen. `gl.viewport` + `gl.scissor` per pane is what
// makes the single context serve N of them, and it costs one state change and one quad per tile:
// 48 panes rendered at **p95 2.2 ms**, flat in pane count. The renderer is not the cost here; tile
// production is, at 11.4 ms per 256² tile server-side (T-438), which is why the interesting code is
// in tilecache.ts.
//
// One ramp. The colour comes from `ui/src/cmap.ts`'s `CMAP_GLSL`, generated from the same
// `CMAP_STOPS` array the 2-D canvas paths read through `cmapBytes` — T-397 built it that way after
// the navigator strips grew a second ramp that stopped at cyan. One renderer must not undo that, so
// this shader imports the ramp rather than writing one out.
//
// One grey. Every cell colour goes through `CELL_RULE_GLSL`, generated from `CELL_MARKS`, and the
// grey appears in exactly one branch of it: the cell whose *coverage state* says `unobserved`. A
// tile that is merely **not resident** is drawn by a different branch entirely — an upscaled
// ancestor, said so, or the pending mark — because a memory budget must never be able to claim the
// radio never looked (T-437 F3).
//
// Presentation only. Tile extents, levels and states all come from the backend; this file maps them
// to pixels and colours.

import { CMAP_GLSL } from "../cmap";
import { BACKDROP, CELL, CELL_RULE_GLSL, PENDING, type DrawKind } from "./cellrule";
import {
  ancestorsOf, extentOf, keyOf, levelsFor, tilesFor,
  type Box, type Lattice, type TileAddr,
} from "./lattice";
import { TileCache, type TileEntry, type TileTextures } from "./tilecache";
import type { TileData } from "./tile";

/** A pane's pixel rectangle, in **GL convention**: origin at the bottom-left of the drawing buffer. */
export interface PaneRect { readonly x: number; readonly y: number; readonly w: number; readonly h: number }

/**
 * A viewport onto the one surface. T-442 owns the user-facing pane model (split, per-pane follow,
 * per-pane pause); this is the minimum the renderer needs, and deliberately carries no mode: a
 * pane is a `(frequency range × time range)` box and a rectangle to draw it in.
 */
export interface PaneView {
  readonly id: string;
  readonly rect: PaneRect;
  readonly box: Box;
  /** Whose coverage decides this pane's grey. Default `any` (the union). */
  readonly device?: string;
}

/** What one pane drew, this frame. `levelF`/`levelT` are §8.5a's "state the level per pane": two
 * viewports at different levels legitimately differ, and the fix is to say so, not to hide it. */
export interface PaneReport {
  readonly id: string;
  readonly levelF: number;
  readonly levelT: number;
  readonly tiles: number;
  readonly fallbacks: number;
  readonly pending: number;
}

const KIND_TILE = 0, KIND_FLAT = 1;

const VS = `#version 300 es
precision highp float;
uniform vec4 uRect;      // x0,y0,x1,y1 in the pane's clip space
uniform vec2 uUv0, uUv1; // the sub-rect of the tile this quad shows
out vec2 vUv;
out vec2 vQ;
void main() {
  vec2 q = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
  vQ = q;
  vUv = mix(uUv0, uUv1, q);
  gl_Position = vec4(mix(uRect.xy, uRect.zw, q), 0.0, 1.0);
}`;

const FS = `#version 300 es
precision highp float;
precision highp sampler2D;
in vec2 vUv;
in vec2 vQ;
out vec4 frag;
uniform sampler2D uValue;   // R16F: the measurement, dB
uniform sampler2D uState;   // R8: the coverage state (ui/src/surface/cellrule.ts)
uniform int   uKind;        // 0 = tile, 1 = flat
uniform vec3  uFlat;
uniform float uLo, uHi;     // ONE display range, shared by every pane: "same ramp, same scale"
uniform float uFallback;    // 1 when this quad is an upscaled coarser ancestor standing in
uniform vec2  uSizePx;      // the quad's size in device px, so the fallback hatch keeps its weight
uniform int   uTier;        // 0 live-iq, 1 spectrum-history, 2 survey-overview — T-441 draws these
${CMAP_GLSL}
${CELL_RULE_GLSL}
void main() {
  if (uKind == ${KIND_FLAT}) { frag = vec4(uFlat, 1.0); return; }
  int s = int(floor(texture(uState, vUv).r * 255.0 + 0.5));
  float v = texture(uValue, vUv).r;
  vec3 col = cellMark(s, (v - uLo) / max(uHi - uLo, 1e-6));
  // A stand-in for a tile that has not arrived is MARKED, never passed off as the level it stands
  // in for (docs/16 §5.5: "draws the coarser parent upscaled and says so"). A coarse diagonal hatch
  // at constant screen weight, so it reads at any zoom and cannot be mistaken for structure.
  if (uFallback > 0.5) {
    vec2 px = vQ * uSizePx;
    col = mix(col, vec3(0.5), 0.10);
    if (fract((px.x + px.y) / 10.0) < 0.35) col *= 0.80;
  }
  frag = vec4(col, 1.0);
}`;

/** The WebGL2 subset this renderer uses. Typed structurally so ui/test can drive it with a stub. */
type GL = WebGL2RenderingContext;

function compile(gl: GL, type: number, src: string): WebGLShader {
  const s = gl.createShader(type)!;
  gl.shaderSource(s, src);
  gl.compileShader(s);
  if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(`${gl.getShaderInfoLog(s)}\n${src}`);
  return s;
}

/** A tile's pair of textures. Two planes because the wire has two planes (ui/src/surface/tile.ts). */
export interface TilePlanes { readonly value: WebGLTexture; readonly state: WebGLTexture }

/** Uploads decoded tiles as an R16F measurement plane and an R8 state plane: 3 bytes a cell. */
export class GlTileTextures implements TileTextures<TilePlanes> {
  constructor(private readonly gl: GL) {}

  upload(data: TileData): TilePlanes {
    const gl = this.gl;
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    const value = this.plane(gl.R16F, data.nf, data.nt);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, data.nf, data.nt, gl.RED, gl.FLOAT, data.value);
    const state = this.plane(gl.R8, data.nf, data.nt);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, data.nf, data.nt, gl.RED, gl.UNSIGNED_BYTE, data.state);
    return { value, state };
  }

  destroy(t: TilePlanes): void {
    this.gl.deleteTexture(t.value);
    this.gl.deleteTexture(t.state);
  }

  private plane(fmt: number, w: number, h: number): WebGLTexture {
    const gl = this.gl, t = gl.createTexture()!;
    gl.bindTexture(gl.TEXTURE_2D, t);
    gl.texStorage2D(gl.TEXTURE_2D, 1, fmt, w, h);
    // NEAREST everywhere: a cell is a measurement over an extent, and interpolating between two of
    // them would invent values at the boundary where the fold already said what it knows.
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return t;
  }
}

export interface SurfaceOptions {
  /** How many levels coarser a fallback may be found. Beyond this the place stays pending, which is
   * honest: an eight-times-coarser cell standing in for a missing one says almost nothing. */
  maxFallbackSteps?: number;
  /** Prefetch the parent level covering each pane (§5.5's second pin): what makes a zoom-out draw
   * instead of flash. */
  pinParents?: boolean;
}

/**
 * One canvas, one context, N panes.
 *
 * `render(panes)` is the whole API: it resolves each pane's own per-axis levels, draws every tile
 * of that pane's box, and reports what it drew. Nothing is retained between frames except the
 * cache, so a pane that moves cannot leave a stale overlay behind — the T-388 class of defect
 * (a per-poll layout against a per-frame scroll) is unrepresentable when every frame re-derives
 * every rectangle from the pane's own box.
 */
export class Surface {
  readonly gl: GL;
  readonly cache: TileCache<TilePlanes>;
  /** The one display range every pane's colour is relative to. */
  lo = -120;
  hi = -60;
  /** Track the observed range the tiles themselves report. One range for every pane, always. */
  autoScale = true;
  drawCalls = 0;
  frames = 0;
  lastFrame: PaneReport[] = [];
  private prog: WebGLProgram;
  private u: Record<string, WebGLUniformLocation | null> = {};
  private vao: WebGLVertexArrayObject;
  private readonly maxFallbackSteps: number;
  private readonly pinParents: boolean;

  constructor(
    readonly canvas: HTMLCanvasElement,
    private lattice: Lattice,
    cache: TileCache<TilePlanes> | ((tex: TileTextures<TilePlanes>) => TileCache<TilePlanes>),
    opts: SurfaceOptions = {},
  ) {
    const gl = canvas.getContext("webgl2", { antialias: false, alpha: false }) as GL | null;
    if (!gl) throw new Error("WebGL2 unavailable");
    this.gl = gl;
    this.maxFallbackSteps = opts.maxFallbackSteps ?? 3;
    this.pinParents = opts.pinParents ?? true;
    const p = gl.createProgram()!;
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, VS));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, FS));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(String(gl.getProgramInfoLog(p)));
    this.prog = p;
    for (const n of ["uRect", "uUv0", "uUv1", "uValue", "uState", "uKind", "uFlat", "uLo", "uHi", "uFallback", "uSizePx", "uTier"]) {
      this.u[n] = gl.getUniformLocation(p, n);
    }
    this.vao = gl.createVertexArray()!;
    this.cache = typeof cache === "function" ? cache(new GlTileTextures(gl)) : cache;
  }

  /** The lattice both axes are addressed on. Set once from a probe; changing it drops nothing —
   * the cache keys carry the scheme, so tiles of two lattices cannot be confused. */
  setLattice(lat: Lattice): void { this.lattice = lat; }
  get lat(): Lattice { return this.lattice; }

  /** A fixed display range for every pane (turns [[autoScale]] off). */
  setScale(lo: number, hi: number): void {
    if (Number.isFinite(lo) && Number.isFinite(hi) && hi > lo) { this.lo = lo; this.hi = hi; this.autoScale = false; }
  }

  /**
   * Draw one frame.
   *
   * The order inside a pane is: resolve the pane's own two levels; ask the cache for each tile in
   * its box; draw resident ones; for a missing one look for a resident coarser ancestor and draw
   * *that*, marked; otherwise draw the pending mark. **Grey is never a choice made here** — it can
   * only come out of a resident tile's state plane.
   */
  render(panes: readonly PaneView[]): PaneReport[] {
    const gl = this.gl;
    this.cache.beginFrame();
    this.frames++;
    this.drawCalls = 0;
    gl.bindVertexArray(this.vao);
    gl.useProgram(this.prog);
    gl.disable(gl.DEPTH_TEST);
    gl.disable(gl.BLEND);
    gl.disable(gl.SCISSOR_TEST);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.clearColor(BACKDROP[0], BACKDROP[1], BACKDROP[2], 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.uniform1f(this.u.uLo, this.lo);
    gl.uniform1f(this.u.uHi, this.hi);

    const reports: PaneReport[] = [];
    let lo = Infinity, hi = -Infinity;
    for (const pane of panes) {
      const r = pane.rect;
      if (!(r.w > 0) || !(r.h > 0)) continue;
      gl.viewport(r.x, r.y, r.w, r.h);
      gl.enable(gl.SCISSOR_TEST);
      gl.scissor(r.x, r.y, r.w, r.h);
      // The pane's ground is PENDING, not grey: before any tile arrives the honest statement is
      // "not loaded", and a grey clear would say "never observed" for the whole pane. The spike
      // cleared panes to grey; that is precisely finding F3, one line long.
      gl.clearColor(PENDING[0], PENDING[1], PENDING[2], 1);
      gl.clear(gl.COLOR_BUFFER_BIT);

      const { levelF, levelT } = levelsFor(this.lattice, pane.box, r.w, r.h);
      const addrs = tilesFor(this.lattice, pane.box, levelF, levelT, pane.device ?? "any");
      let tiles = 0, fallbacks = 0, pending = 0;
      for (const a of addrs) {
        const res = this.cache.acquire(a);
        if (res.kind === "resident") {
          this.drawRegion(pane, extentOf(this.lattice, a), res.entry, "tile", r);
          const rg = res.entry.data.rangeDb;
          if (rg) { lo = Math.min(lo, rg.lo); hi = Math.max(hi, rg.hi); }
          tiles++;
          continue;
        }
        const stand = this.fallbackFor(a);
        if (stand) { this.drawRegion(pane, extentOf(this.lattice, a), stand, "fallback", r); fallbacks++; }
        else { this.drawFlat(pane, extentOf(this.lattice, a), PENDING, r); pending++; }
      }
      // §5.5's second pin, and a **coarse-first fill**. These go on the queue *after* the pane's
      // own tiles, and the queue is LIFO, so a parent is fetched FIRST — deliberately: at 11.4 ms a
      // tile one parent covers four children's worth of screen through the fallback path, so a cold
      // viewport shows something honest in a quarter of the time. It is also what makes a zoom-out
      // draw instead of flash.
      if (this.pinParents && levelF + 1 < this.lattice.levelsF) {
        for (const a of tilesFor(this.lattice, pane.box, levelF + 1, Math.min(levelT + 1, this.lattice.levelsT - 1), pane.device ?? "any")) {
          this.cache.prefetch(a);
        }
      }
      reports.push({ id: pane.id, levelF, levelT, tiles, fallbacks, pending });
    }
    gl.disable(gl.SCISSOR_TEST);
    if (this.autoScale && lo < hi) {
      // One range for every pane, moved gently: two panes showing the same energy must not read as
      // two strengths on one screen (T-397's honesty problem, one layer up).
      this.lo += 0.15 * (lo - 8 - this.lo);
      this.hi += 0.15 * (hi + 3 - this.hi);
    }
    this.cache.setViewports(this.lattice, panes.map((p) => p.box));
    this.cache.endFrame();
    this.lastFrame = reports;
    return reports;
  }

  /** The nearest resident coarser tile containing `a`, or null. Peeks only: the ancestor search
   * must not enqueue a fetch at every level it tries, or one miss becomes `maxFallbackSteps²`. */
  private fallbackFor(a: TileAddr): TileEntry<TilePlanes> | null {
    for (const anc of ancestorsOf(this.lattice, a, this.maxFallbackSteps)) {
      const e = this.cache.peek(anc, true);
      if (e) return e;
    }
    return null;
  }

  /** Draws `region` of the surface from `entry`'s texture — the whole tile when they coincide, a
   * sub-rect when an ancestor is standing in for one of its children. */
  private drawRegion(pane: PaneView, region: Box, entry: TileEntry<TilePlanes>, kind: DrawKind, rect: PaneRect): void {
    const gl = this.gl;
    const tex = extentOf(this.lattice, entry.addr);
    const clip = toClip(region, pane.box);
    const u0 = (region.f0Hz - tex.f0Hz) / (tex.f1Hz - tex.f0Hz);
    const u1 = (region.f1Hz - tex.f0Hz) / (tex.f1Hz - tex.f0Hz);
    const v0 = (region.t0Ns - tex.t0Ns) / (tex.t1Ns - tex.t0Ns);
    const v1 = (region.t1Ns - tex.t0Ns) / (tex.t1Ns - tex.t0Ns);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, entry.tex.value);
    gl.uniform1i(this.u.uValue, 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, entry.tex.state);
    gl.uniform1i(this.u.uState, 1);
    gl.uniform1i(this.u.uKind, KIND_TILE);
    gl.uniform1f(this.u.uFallback, kind === "fallback" ? 1 : 0);
    gl.uniform1i(this.u.uTier, entry.data.tier === "live-iq" ? 0 : entry.data.tier === "spectrum-history" ? 1 : 2);
    gl.uniform4f(this.u.uRect, clip[0], clip[1], clip[2], clip[3]);
    gl.uniform2f(this.u.uUv0, u0, v0);
    gl.uniform2f(this.u.uUv1, u1, v1);
    gl.uniform2f(this.u.uSizePx, ((clip[2] - clip[0]) / 2) * rect.w, ((clip[3] - clip[1]) / 2) * rect.h);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    this.drawCalls++;
  }

  /** A flat mark over `region`: [[PENDING]] for a tile that has not arrived. Never grey. */
  private drawFlat(pane: PaneView, region: Box, rgb: readonly [number, number, number], rect: PaneRect): void {
    const gl = this.gl;
    const clip = toClip(region, pane.box);
    gl.uniform1i(this.u.uKind, KIND_FLAT);
    gl.uniform1f(this.u.uFallback, 0);
    gl.uniform3f(this.u.uFlat, rgb[0], rgb[1], rgb[2]);
    gl.uniform4f(this.u.uRect, clip[0], clip[1], clip[2], clip[3]);
    gl.uniform2f(this.u.uSizePx, ((clip[2] - clip[0]) / 2) * rect.w, ((clip[3] - clip[1]) / 2) * rect.h);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    this.drawCalls++;
  }

  dispose(): void {
    this.cache.dispose();
    this.gl.getExtension("WEBGL_lose_context")?.loseContext();
  }
}

/**
 * A region of the surface in a pane's clip space. Time runs **down** the pane — the newest row at
 * the top — so `t1` maps to clip `+1`, which is the same convention the waterfall has always drawn
 * with and the one every overlay places itself through (T-337's one shared time axis).
 */
export function toClip(region: Box, box: Box): [number, number, number, number] {
  const fx = (f: number) => (2 * (f - box.f0Hz)) / (box.f1Hz - box.f0Hz) - 1;
  const ty = (t: number) => (2 * (t - box.t0Ns)) / (box.t1Ns - box.t0Ns) - 1;
  return [fx(region.f0Hz), ty(region.t0Ns), fx(region.f1Hz), ty(region.t1Ns)];
}

/** Re-exported so a caller need not reach past the renderer for the states it reports on. */
export { CELL, keyOf };
