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
// One scale, and it is **not the viewport's** (T-470). The ramp needs a `(lo, hi)` to be a colour at
// all, and until T-470 this file computed that pair, every frame, from the `range_db` of the tiles
// that happened to be *on screen*. Zooming changes which tiles those are, so it changed the range,
// so **the same measured dB re-coloured** — the user's report was "colours animate and shift when I
// zoom", and the reading behind it ("it is normalised to the visible time/region") was exactly
// right. The default is now **anchored**: one `(lo, hi)` measured once over the region by the
// backend (`GET /api/coverage`'s `shade.range_db`, whose `normalisation` is this shader's
// arithmetic in so many words) and held. Same measurement, same colour, at every zoom — the
// slippy-map property, on the colour axis.
//
// Auto-scale survives as an **opt-in** ([[Surface.setAutoScale]]): tracking the visible tiles is a
// genuinely useful contrast control for digging into weak signals, and the trade a fixed range makes
// — it **can** clip a strong signal or wash out a weak one — is real. It is stated in the legend
// rather than hidden, which is what keeps a consistent picture an honest one. What it may not be is
// the default, because a default that re-colours on navigation makes colour unreadable as a
// quantity.
//
// One grey. Every cell colour goes through `CELL_RULE_GLSL`, generated from `CELL_MARKS`, and the
// grey appears in exactly one branch of it: the cell whose *coverage state* says `unobserved`. A
// tile that is merely **not resident** is drawn by a different branch entirely — an upscaled
// ancestor, said so, or the pending mark — because a memory budget must never be able to claim the
// radio never looked (T-437 F3).
//
// One place for every mark. T-441 extended that generation to the three honesty tiers (§8.3: they
// "stay visually distinct … so a wide or deep zoom never fakes resolution the hardware did not
// capture") and to the stand-in hatch, so this file now contains no colour arithmetic of its own at
// all — only the decision of *which* rule applies to a quad, and the pixel geometry each rule needs.
// The tier mark qualifies a **measurement** and is applied only to an `OBSERVED` cell; the
// survey-overview lattice is drawn at `uSrcPx`, the on-screen size of a cell the front end really
// measured, which is how replication is declared rather than smoothed over.
//
// Presentation only. Tile extents, levels and states all come from the backend; this file maps them
// to pixels and colours.

import { CMAP_GLSL } from "../cmap";
import { BACKDROP, CELL, CELL_RULE_GLSL, PENDING, SHADOW_MARK, tierByte, type DrawKind } from "./cellrule";
import {
  ancestorsOf, extentOf, keyOf, levelsFor, tilesFor,
  type Box, type Lattice, type TileAddr,
} from "./lattice";
import { TileCache, type TileEntry, type TileTextures, type Viewport } from "./tilecache";
import type { TileData } from "./tile";
import { HEADROOM_DB, MIN_SPAN_DB, ViewportScale } from "./vscale";

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
  /**
   * **Does what this viewport shows decide the shared display range?** Default `true`; the minimap
   * passes `false` (T-528).
   *
   * It exists because the minimap is a viewport over the *whole surface*. In the viewport-dynamic
   * mode that would make "the range of what is on screen" mean "the range of the entire survey" at
   * all times, and the mode would silently do nothing — the quiet band the user zoomed into would
   * still be scaled against a carrier 4 GHz away, which is precisely the complaint. The map is
   * where you see *where the panes are*, not a subject (`view.ts`'s own words for why it is capped
   * at half the canvas), so it is coloured by the panes' scale rather than deciding it.
   *
   * It is read **only** by the viewport mode. `auto` still takes the union over every viewport
   * including the map, unchanged, because that mode's own claim is about tiles on the canvas and
   * T-470's browser-tier measurements are calibrated against it.
   */
  readonly scales?: boolean;
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
  /** Places drawn as [[REFUSED_MARK]]: asked for, and no usable answer came back (T-499). They are
   * **not** counted in `pending` — "wait" and "nothing is coming" are different states, and a
   * readout that folds them together is the progress bar that never finishes. */
  readonly refused: number;
}

const KIND_TILE = 0, KIND_FLAT = 1, KIND_REFUSED = 2;

/**
 * How the one display range was decided. `anchored` is the default and is zoom-invariant.
 *
 * - `anchored` — one `(lo, hi)` measured once over the region and held (T-470). The same measured
 *   dB is the same colour at every zoom, and anything outside the range clips.
 * - `auto` — the union of the `range_db` **of the tiles on screen**. Cheap, nothing clips, and the
 *   tile is the granularity: a carrier off screen in a tile that is partly on screen still sets the
 *   top of the ramp.
 * - `viewport` — measured from the **observed cells actually inside the viewport** (T-528), shadow
 *   cells excluded. What a user means by "scale to what I am looking at": a quiet band spreads
 *   across the ramp instead of sitting in the bottom few percent. See `./vscale.ts`.
 */
export type RangeMode = "anchored" | "auto" | "viewport";

/** The display range, and **where it came from** — a range with no provenance is a number a user
 * cannot check. Every surface that states the scale states this whole record. */
export interface DisplayRange {
  readonly lo: number;
  readonly hi: number;
  readonly mode: RangeMode;
  /** One clause naming the measurement (or the fallback) this range is. */
  readonly source: string;
}

/**
 * The range used when **nothing measured one**.
 *
 * It is reached only when no backend answer carried a `range_db` at all, which is very nearly the
 * same condition as *nothing here was ever observed* — and a surface with no observed cell has
 * nothing on the ramp to mis-colour. So this constant is a stated fallback for an empty screen, not
 * a guess competing with a measurement, and [[DisplayRange.source]] says which of the two is in
 * force. The span is 60 dB, the working dynamic range of an 8-bit front end.
 */
export const FALLBACK_RANGE = { lo: -100, hi: -40 } as const;
export const FALLBACK_RANGE_SOURCE =
  "no measured range was reported: this client's stated 60 dB fallback, not a measurement";

/**
 * **The anchored display range's span, in dB.** The top is measured; this is how far below it the
 * ramp reaches.
 *
 * Why the bottom is stated rather than measured, when the backend reports a `range_db` with both
 * ends. The fold is **max-hold**, so the two ends are not the same kind of number:
 *
 * - `range_db.hi` is a maximum of maxima, and folding further never lowers a maximum. The coarse
 *   answer therefore equals the fine one **exactly**, at any resolution, over the same region. It is
 *   a measurement this client can adopt as-is.
 * - `range_db.lo` is the *minimum of the cells' maxima*, and folding coarser can only **raise** it.
 *   It is an upper bound on the floor, not the floor — and the gap is large: over this project's FM
 *   fixture the 128 × 32 coverage grid reports −72 dBFS where the level-0 cells the waterfall draws
 *   reach −86. Anchoring the bottom there clips 14 dB of real measurement to black, which was
 *   measured on the spectrum trace: **586 of 820 drawn columns collapsed to 44**, every one of them
 *   pinned to the floor of the strip. A scale that hides a third of the dynamic range is not a
 *   trade-off, it is a broken picture.
 *
 * So the top is the measurement and the span is a **stated** 60 dB — the working dynamic range of an
 * 8-bit front end, and the "fixed dynamic span" T-470 asked for. Nothing above the range exists by
 * construction, and 60 dB below a region's own peak reaches past any noise floor this front end can
 * show, so in practice the anchored ramp clips neither end while still being a fixed scale.
 */
export const ANCHOR_SPAN_DB = 60;

/**
 * The provenance of a [[RangeMode]] `viewport` range (T-528), in its three states.
 *
 * It is written here rather than in the legend because it is a claim about a **measurement this
 * renderer took**, and the one rule the display range has never been allowed to break is that the
 * number and the account of where it came from travel together. The mode moves the range as the
 * view moves, so the sentence moves with it: it names the cells it measured and, when it widened a
 * nearly-flat viewport to [[MIN_SPAN_DB]], says that too — a stretched flat band that did not say
 * it was stretched would be structure invented out of quantisation.
 */
export const VIEWPORT_SOURCE_PENDING =
  "auto-contrast (viewport): measured from the observed cells on screen, shadows excluded — no frame drawn yet";
export const VIEWPORT_SOURCE_EMPTY =
  "auto-contrast (viewport): no observed cell is on screen, so the last measured range is held";
function viewportSource(blocks: number, r: { lo: number; hi: number }): string {
  const flat = r.hi - r.lo <= MIN_SPAN_DB + 1e-6;
  return `auto-contrast (viewport): measured over ${blocks} block${blocks === 1 ? "" : "s"} of observed cells`
    + ` now on screen (shadows excluded, ±${HEADROOM_DB} dB headroom)`
    + (flat ? `, widened to the stated ${MIN_SPAN_DB} dB minimum because the viewport is flat` : "");
}

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
uniform vec2  uSizePx;      // the quad's size in device px, so every mark keeps its screen weight
uniform int   uTier;        // 0 live-iq, 1 spectrum-history, 2 survey-overview (cellrule.ts's TIER)
uniform vec2  uSrcPx;       // the on-screen size of one cell the front end ACTUALLY measured
uniform float uShadowGain;  // the shadow's brightness multiplier (T-526): client-adjustable, default
                             // SHADOW_MARK.gain (0.25) from ./shadow-gain.ts; the shadow shape itself
                             // (scanlines, which ramp) stays whatever CELL_MARKS says
${CMAP_GLSL}
${CELL_RULE_GLSL}
void main() {
  if (uKind == ${KIND_FLAT}) { frag = vec4(uFlat, 1.0); return; }
  // **Asked, and no usable answer came back** (T-499). A statement about this client's last request,
  // so it is drawn from its own mark and never from a cell state: nothing here can reach cellMark,
  // and therefore nothing here can produce THE grey.
  if (uKind == ${KIND_REFUSED}) { frag = vec4(refusedMark(vQ * uSizePx), 1.0); return; }
  vec2 px = vQ * uSizePx;
  int s = int(floor(texture(uState, vUv).r * 255.0 + 0.5));
  float v = texture(uValue, vUv).r;
  vec3 col = cellMark(s, (v - uLo) / max(uHi - uLo, 1e-6), px, uShadowGain);
  // **The honesty tier qualifies a measurement and nothing else** (docs/16 §8.3). Only an OBSERVED
  // cell carries a resolution claim to overstate; a tier wash over an unobserved cell would make a
  // second grey, which is the one thing this shader may not contain.
  if (s == ${CELL.OBSERVED}) col = tierMark(uTier, col, px, uSrcPx);
  // A stand-in for a tile that has not arrived is MARKED, never passed off as the level it stands
  // in for (docs/16 §5.5: "draws the coarser parent upscaled and says so"). A coarse diagonal hatch
  // at constant screen weight, so it reads at any zoom and cannot be mistaken for structure.
  if (uFallback > 0.5) col = fallbackMark(col, px);
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
  lo: number = FALLBACK_RANGE.lo;
  hi: number = FALLBACK_RANGE.hi;
  /** Which of the three ways [[lo]]/[[hi]] is being decided. `anchored` unless a host opts out. */
  private mode: RangeMode = "anchored";
  /**
   * **Opt-in** (T-470). On, the range tracks the `range_db` of the tiles currently on screen, so the
   * same measured dB changes colour as the viewport changes — useful as a manual contrast control,
   * wrong as a default. Off (the default) the range is whatever [[setScale]] anchored it to.
   *
   * A derived reading of [[mode]] since T-528: it is `true` for `auto` and **false for `viewport`**,
   * because the tile-`range_db` branch in [[render]] is exactly what the viewport mode replaces.
   */
  get autoScale(): boolean { return this.mode === "auto"; }
  /**
   * The range this frame's screen asked for, applied at the **start** of the next [[render]].
   *
   * Why it is not applied where it is computed. The scale a frame was drawn with is uploaded before
   * the first quad; a range computed from that same frame's geometry can only be known once every
   * quad has been resolved. Assigning it at the end of `render` therefore leaves [[lo]]/[[hi]] —
   * what the legend, the readout and the trace strip's y axis all quote — describing a frame that
   * has not been drawn yet, which is T-475's rule ("the readout's numbers must be the pixels' own")
   * broken by one frame. Holding it here instead means [[lo]]/[[hi]] are **always** the pair the
   * last frame was drawn with, and the geometry the range came from is one frame old rather than
   * the statement being one frame early. A frame at 60 Hz is not visible; a wrong number is.
   */
  private next: { lo: number; hi: number; source: string } | null = null;
  /** Where [[lo]]/[[hi]] came from, so every surface that states the range can state its provenance. */
  rangeSource: string = FALLBACK_RANGE_SOURCE;
  /** The shadow's brightness multiplier (T-526), a per-viewer display preference — set via
   * [[setShadowGain]], never fetched. Defaults to [[SHADOW_MARK]]'s own gain. */
  shadowGain: number = SHADOW_MARK.gain;
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
    for (const n of ["uRect", "uUv0", "uUv1", "uValue", "uState", "uKind", "uFlat", "uLo", "uHi", "uFallback", "uSizePx", "uTier", "uSrcPx", "uShadowGain"]) {
      this.u[n] = gl.getUniformLocation(p, n);
    }
    this.vao = gl.createVertexArray()!;
    this.cache = typeof cache === "function" ? cache(new GlTileTextures(gl)) : cache;
  }

  /** The lattice both axes are addressed on. Set once from a probe; changing it drops nothing —
   * the cache keys carry the scheme, so tiles of two lattices cannot be confused. */
  setLattice(lat: Lattice): void { this.lattice = lat; }
  get lat(): Lattice { return this.lattice; }

  /**
   * **Anchor** the display range: one `(lo, hi)` for every pane, held whatever the viewport does
   * (and so it turns [[autoScale]] off).
   *
   * `source` is not decoration. A fixed range can clip or wash out, and the only thing that makes
   * that honest rather than merely consistent is a legend that can say *which measurement this
   * scale is* — so the provenance travels with the numbers instead of being reconstructed by
   * whoever draws the key.
   */
  setScale(lo: number, hi: number, source = "set by the host"): void {
    if (Number.isFinite(lo) && Number.isFinite(hi) && hi > lo) {
      this.lo = lo;
      this.hi = hi;
      this.mode = "anchored";
      this.next = null;
      this.rangeSource = source;
    }
  }

  /**
   * Turn the opt-in contrast tracker on or off.
   *
   * Turning it **off** leaves the range exactly where tracking left it — it freezes the picture the
   * user was looking at rather than jumping back to the anchor, which is what a contrast control
   * should do. `setScale` is how a caller returns to a stated anchor.
   */
  setAutoScale(on: boolean): void {
    this.setRangeMode(on ? "auto" : "anchored");
  }

  /**
   * Choose how the range is decided (T-528). Like [[setAutoScale]], leaving a tracking mode holds
   * the range where tracking left it rather than jumping: the picture on screen does not change
   * under the press, only what will move it next.
   */
  setRangeMode(mode: RangeMode): void {
    this.mode = mode;
    this.next = null;
    if (mode === "auto") this.rangeSource = "auto-contrast: the range of the tiles currently on screen";
    else if (mode === "viewport") this.rangeSource = VIEWPORT_SOURCE_PENDING;
    else this.rangeSource = `held at ${this.lo.toFixed(1)}…${this.hi.toFixed(1)} dBFS, where auto-contrast left it`;
  }

  /** The one display range, with its provenance. What a legend states, in every mode. */
  get range(): DisplayRange {
    return { lo: this.lo, hi: this.hi, mode: this.mode, source: this.rangeSource };
  }

  /** Set the shadow's brightness multiplier (clamped by the caller — `./shadow-gain.ts`'s
   * `clampShadowGain`), a display-only change that needs no refetch: the next `render()` uses it. */
  setShadowGain(gain: number): void { this.shadowGain = gain; }

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
    // **Apply the range the previous frame asked for, before a single quad is drawn.** See
    // [[next]]: this is what makes `lo`/`hi` the pair the frame really was drawn with, for the
    // legend, the readout and the trace strip that all read them after `render` returns.
    if (this.next) {
      this.lo = this.next.lo;
      this.hi = this.next.hi;
      this.rangeSource = this.next.source;
      this.next = null;
    }
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
    gl.uniform1f(this.u.uShadowGain, this.shadowGain);

    const reports: PaneReport[] = [];
    // The boxes AND the levels each was drawn at, for the cache's cancellation predicate: a box on
    // its own cannot say a tile is no longer wanted once one viewport (the minimap) is the size of
    // the surface. See TileCache.setViewports.
    const viewports: Viewport[] = [];
    let lo = Infinity, hi = -Infinity;
    // T-528's accumulator, built fresh every frame and discarded with it — nothing about the range
    // may outlive the geometry it was measured over. `null` in every other mode, so the per-cell
    // path costs exactly nothing unless the user asked for it.
    const vscale = this.mode === "viewport" ? new ViewportScale() : null;
    for (const pane of panes) {
      // The map is a viewport over the whole surface, so it may not be the thing the scale is
      // measured over. See [[PaneView.scales]].
      const measure = vscale && pane.scales !== false ? vscale : null;
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
      viewports.push({ box: pane.box, levelF, levelT });
      const addrs = tilesFor(this.lattice, pane.box, levelF, levelT, pane.device ?? "any");
      let tiles = 0, fallbacks = 0, pending = 0, refused = 0;
      for (const a of addrs) {
        const res = this.cache.acquire(a);
        if (res.kind === "resident") {
          const region = extentOf(this.lattice, a);
          this.drawRegion(pane, region, res.entry, "tile", r);
          // The cells of this tile that are inside this pane's box — the measurement the viewport
          // mode is a scale over. A resident tile draws its own extent, so the texture's extent and
          // the region are the same box.
          measure?.add(res.entry.data, region, region, pane.box);
          // **The one read of a tile's own range, and it is inside the opt-in branch** (T-470). What
          // is on screen decides the scale only when the user has asked for that; otherwise the
          // scale is anchored and this loop cannot touch it. `ui/test/surface-range.test.ts` asserts
          // that on this source, because the defect is re-introducible in one line.
          if (this.autoScale) {
            const rg = res.entry.data.rangeDb;
            if (rg) { lo = Math.min(lo, rg.lo); hi = Math.max(hi, rg.hi); }
          }
          tiles++;
          continue;
        }
        const stand = this.fallbackFor(a);
        if (stand) {
          const region = extentOf(this.lattice, a);
          this.drawRegion(pane, region, stand, "fallback", r);
          // A coarse stand-in's cells ARE what is on the screen here, so they count — but the
          // texture is the ancestor's, so the visible sub-rect is mapped through the ancestor's own
          // extent, not the child's. Getting that pair the wrong way round would read a different
          // corner of the ancestor than the one being displayed.
          measure?.add(stand.data, extentOf(this.lattice, stand.addr), region, pane.box);
          fallbacks++;
        }
        // **`pending` and `refused` are drawn apart** (T-499). A place with no usable answer is not
        // waiting for one — the route refused it, or the server is unreachable and this client is
        // backing off — and painting it as PENDING is a progress bar that never finishes. Neither
        // branch can produce grey: both are [[DrawKind]]s, and grey comes only from a state byte.
        else if (res.failed) { this.drawRefused(pane, extentOf(this.lattice, a), r); refused++; }
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
      reports.push({ id: pane.id, levelF, levelT, tiles, fallbacks, pending, refused });
    }
    gl.disable(gl.SCISSOR_TEST);
    if (this.autoScale && lo < hi) {
      // One range for every pane, moved gently: two panes showing the same energy must not read as
      // two strengths on one screen (T-397's honesty problem, one layer up). This is the **opt-in**
      // contrast control since T-470 — as a default it is the defect, because the "energy" whose
      // colour it holds steady across panes is not held steady across *zooms*.
      this.next = {
        lo: this.lo + 0.15 * (lo - 8 - this.lo),
        hi: this.hi + 0.15 * (hi + 3 - this.hi),
        source: this.rangeSource,
      };
    }
    if (vscale) {
      // **One range for every pane here too**, measured over the union of what all of them are
      // showing. A per-pane scale would put the same dB on two colours on one screen, which is the
      // thing this renderer has one `(uLo, uHi)` to prevent; the mode changes *which* measurement
      // decides the pair, not how many pairs there are.
      const want = vscale.range();
      // Nothing observed on screen is a real state of this surface, not an empty measurement: hold
      // the range and say so, rather than stretch a ramp over grey.
      this.next = want
        ? { ...want, source: viewportSource(vscale.blocks, want) }
        : { lo: this.lo, hi: this.hi, source: VIEWPORT_SOURCE_EMPTY };
    }
    this.cache.setViewports(this.lattice, viewports);
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
    gl.uniform1i(this.u.uTier, tierByte(entry.data.tier));
    gl.uniform4f(this.u.uRect, clip[0], clip[1], clip[2], clip[3]);
    gl.uniform2f(this.u.uUv0, u0, v0);
    gl.uniform2f(this.u.uUv1, u1, v1);
    const wPx = ((clip[2] - clip[0]) / 2) * rect.w, hPx = ((clip[3] - clip[1]) / 2) * rect.h;
    gl.uniform2f(this.u.uSizePx, wPx, hPx);
    const src = sourceCellPx(entry.data, u1 - u0, v1 - v0, wPx, hPx);
    gl.uniform2f(this.u.uSrcPx, src[0], src[1]);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    this.drawCalls++;
  }

  /** A flat mark over `region`: [[PENDING]] for a tile that has not arrived. Never grey. */
  private drawFlat(pane: PaneView, region: Box, rgb: readonly [number, number, number], rect: PaneRect): void {
    const gl = this.gl;
    const clip = toClip(region, pane.box);
    gl.uniform1i(this.u.uKind, KIND_FLAT);
    gl.uniform1f(this.u.uFallback, 0);
    gl.uniform2f(this.u.uSrcPx, 1, 1);
    gl.uniform3f(this.u.uFlat, rgb[0], rgb[1], rgb[2]);
    gl.uniform4f(this.u.uRect, clip[0], clip[1], clip[2], clip[3]);
    gl.uniform2f(this.u.uSizePx, ((clip[2] - clip[0]) / 2) * rect.w, ((clip[3] - clip[1]) / 2) * rect.h);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    this.drawCalls++;
  }

  /** The refused mark over `region` (T-499): asked for, no usable answer. Patterned, so it reads as
   * texture rather than as a level — and, like every mark here, never grey. */
  private drawRefused(pane: PaneView, region: Box, rect: PaneRect): void {
    const gl = this.gl;
    const clip = toClip(region, pane.box);
    gl.uniform1i(this.u.uKind, KIND_REFUSED);
    gl.uniform1f(this.u.uFallback, 0);
    gl.uniform2f(this.u.uSrcPx, 1, 1);
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
 * The on-screen size, in device pixels, of **one cell the front end actually measured** — the pitch
 * the `survey-overview` lattice is drawn at, so a replicated tile shows its real resolution instead
 * of a smooth upscale of it (docs/16 §4: *never imply resolution the front end did not capture*).
 *
 * `du`/`dv` are the fraction of the tile this quad shows, so an ancestor standing in for one of its
 * children gets the pitch of the part on screen rather than of the whole texture.
 *
 * Floored at 2 px: below that a lattice is aliasing rather than information, and a mark nobody can
 * resolve is not a statement. The floor can only make the drawn cells look *finer* than they are on
 * an axis where they are already finer than two pixels — never coarser, which is the direction that
 * would be a claim.
 *
 * **Total, by construction.** A tile whose `measured` is missing or nonsensical falls back to the
 * tile's own served grid, which is the *same* default [[decodeTile]] applies when the answer carries
 * no `fold` block — one rule in one more place, not a second policy. So the drawn pitch is always
 * what the tile itself says, and there is no input for which this throws. That last part is not
 * politeness: `drawRegion` runs inside the frame loop, so a throw here does not spoil one tile, it
 * blanks **every pane on the screen**. A renderer may not have an input that turns the whole surface
 * off.
 */
export function sourceCellPx(
  data: { readonly nf: number; readonly nt: number; readonly measured?: { readonly nf: number; readonly nt: number } },
  du: number, dv: number, wPx: number, hPx: number,
): [number, number] {
  // `measured ?? served` is "no replication was reported", which is exactly what `measured` equal to
  // the served grid means everywhere else in this client.
  const axis = (measured: number | undefined, total: number, d: number, px: number) => {
    const served = Number.isFinite(total) && total > 0 ? total : 1;
    const m = Number.isFinite(measured) && (measured as number) > 0 ? Math.min(measured as number, served) : served;
    return Math.max(2, Math.abs(px) / Math.max(1e-6, m * Math.abs(Number.isFinite(d) ? d : 1)));
  };
  return [
    axis(data.measured?.nf, data.nf, du, wPx),
    axis(data.measured?.nt, data.nt, dv, hPx),
  ];
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
