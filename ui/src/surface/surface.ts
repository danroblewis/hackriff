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
  ancestorsOf, extentOf, keyOf, oneTier, tierFor, tilesFor,
  type Box, type Lattice, type LatticeSet, type TileAddr, type ViewTier,
} from "./lattice";
import { ringCovers, ringPlan, type LiveRingSource, type RingDraw, type RingFrame } from "./livering";
import { lastRowArrival, liveMetrics, paintLatencyMs } from "./livemetrics";
import { TileCache, type TileEntry, type TileTextures, type Viewport } from "./tilecache";
import type { TileData } from "./tile";
import type { Survey } from "./survey";
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
  /**
   * **The pane's coverage-fog layer** (T-807 / MAP-07, docs/24 §13.3). Default `true` (shown). When
   * `false`, the fog states (`cellrule.ts`'s `FOG_STATES`: unobserved, unknown) draw as the flat
   * `FOG_HIDDEN` ground instead of the grey and the hatch. A flag on the one cell rule, read by the
   * one data pass — never a quad and never a second rule. Every measurement-bearing state is drawn
   * either way.
   */
  readonly fog?: boolean;
}

/** What one pane drew, this frame. `levelF`/`levelT` are §8.5a's "state the level per pane": two
 * viewports at different levels legitimately differ, and the fix is to say so, not to hide it. */
export interface PaneReport {
  readonly id: string;
  /**
   * **Which tier this pane drew from** (T-505), and the lattice it was addressed on.
   *
   * §8.5a's rule one level up: a pane states the level it was drawn at, and since the honesty
   * tiers became real in the tile *source* it must also state **which source**. `levelF`/`levelT`
   * are indices into `lat`, so they are meaningless without it — and a caller that read them
   * against a second lattice of its own would be back to two derivations of one picture, which is
   * the T-388 family. Everything downstream (the chrome's cell size, the trace's slice, the tick
   * pitch) reads `lat` from here rather than from the host.
   */
  readonly tier: ViewTier;
  readonly lat: Lattice;
  /** True when the viewport asked for a level past `lat`'s ceiling: the stated reason a pane left
   * the detail tier, and honest to show, since the cells drawn are finer than a pixel. */
  readonly clamped: boolean;
  readonly levelF: number;
  readonly levelT: number;
  readonly tiles: number;
  readonly fallbacks: number;
  readonly pending: number;
  /** Places drawn as [[REFUSED_MARK]]: asked for, and no usable answer came back (T-499). They are
   * **not** counted in `pending` — "wait" and "nothing is coming" are different states, and a
   * readout that folds them together is the progress bar that never finishes. */
  readonly refused: number;
  /**
   * **Resident tiles whose answer does not reach the live edge** (T-532): the copy is in hand, and
   * its newest rows were recorded after it was built, so that strip is left as the pane's PENDING
   * ground rather than drawn from a plane that cannot speak about it (see [[TileData.asOfNs]]).
   *
   * It is a **fourth** count and not part of `pending`, deliberately: the tile arrived, so a
   * readout that called it pending would say the fetch had not landed. On a following pane it is
   * the normal state of the live-edge column and is the number to watch when the edge stops
   * keeping up — `ui/test/surface-edge.test.ts` and the canvas journey both read it.
   */
  readonly behind: number;
  /** Resident tiles whose stated horizon is at or below their own start, so the copy had NOTHING
   * to say about any row on screen and nothing was drawn from them. Counted apart from
   * [[behind]] because the two are different states of the same surface: `behind` still put
   * measured cells on the screen, `blank` left the pane's PENDING ground showing over a tile the
   * client is holding. A rising `blank` is the "we have it but didn't render it" failure. */
  readonly blank: number;
  /** **How far short of its own window top this pane was actually drawn, in ns.** `0` when the
   * tiles in hand reach the top of the pane. Positive when the newest thing drawn is older than
   * the instant the pane is showing — which is what the horizon clip does to a pane whose window
   * runs ahead of `coverage.horizon.as_of_s`. It is the quantity a flat pane is diagnosed by: a
   * pane can be fully resident, fully observed and still be almost entirely its own PENDING
   * ground if this is most of its height. */
  readonly shortNs: number;
  /**
   * **Places answered by the coverage survey and never requested** (T-580): the survey says the
   * radio never sampled this tile's frequencies at any instant up to its end, so it is drawn as THE
   * grey — from an `UNOBSERVED` state byte, up to the survey's horizon — without a tile request.
   * Counted apart from `pending` because nothing is coming: that is the point.
   */
  readonly surveyed: number;
  /**
   * **Tiles drawn in this pane whose last-known (shadow) cells came from a COARSER source than the
   * tile's own level** (T-916) — the spectrum-history ladder's fallback, or a source the answer did
   * not label.
   *
   * It is a resolution statement, not an error count. Since T-911 a recently-departed band's shadow
   * is read at the tile's own level and so is the very cell the band's last live row was drawn
   * with; a band that left longer ago than that search's reach is answered by the ladder, whose
   * max-hold over a ~260× larger box measured 10–15 dB hotter. Both are honest last-known values
   * and neither is grey — but they are not the same resolution, and this surface's rule is that a
   * pane states the level it was actually drawn at.
   */
  readonly shadowLadder: number;
  /** The coarsest such source cell now on screen, `(Hz, s)`; `0` on an axis nothing stated, and
   * both `0` when `shadowLadder` is `0`. */
  readonly shadowCellHz: number;
  readonly shadowCellS: number;
  /**
   * **Rows this pane painted from the live ring** (T-1042, `./livering.ts`): the published
   * `spectrum/live` rows drawn straight at the live edge, so the newest rows are on screen because
   * they were recorded rather than when a tile can be produced for them.
   *
   * `0` on every pane that has no ring — a frozen one, a host that passes none, or a pane zoomed out
   * past the rows' own resolution ([[ringRowPx]] under `MIN_ROW_PX`, where the pyramid's folds are
   * the honest answer). It is the number that says the live lane is working.
   */
  readonly ringRows: number;
  /** **Tile addresses the ring's own extent made unnecessary**: everything this pane shows of them
   * is painted from rows, so they were neither drawn nor requested. Counted apart from `surveyed`
   * (the other not-asked count) because the reason is different: there *is* data, and it is already
   * on the screen from a fresher source. */
  readonly ringTiles: number;
  /** One ring row's height in this pane, device px — the eligibility measurement, reported rather
   * than hidden so a pane that stood the ring aside can say why. `0` with no ring. */
  readonly ringRowPx: number;
  /**
   * **Row t vs rAF** (T-1048 / LSR-7): this draw call's wall clock minus the newest ring row's own
   * ARRIVAL wall clock (`./livemetrics.ts`'s `lastRowArrival` — never the row's capture time, which
   * is not wall-clock-comparable on a replay), ms — the sample→pixel latency the live-rendering
   * invariant is checked against. `null` when this pane drew no NEW live row this frame (no ring, the
   * ring stood aside below `MIN_ROW_PX`, or the newest row was already accounted for by an earlier
   * frame), never a stale number left over from one.
   */
  readonly ringLatencyMs: number | null;
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
                             // SHADOW_MARK.gain (0.32) from ./shadow-gain.ts; the shadow shape itself
                             // (scanlines, which ramp) stays whatever CELL_MARKS says
uniform bool  uFog;         // the pane's coverage-fog layer (T-807): false draws FOG_STATES as FOG_HIDDEN
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
  vec3 col = cellMark(s, (v - uLo) / max(uHi - uLo, 1e-6), px, uShadowGain, uFog);
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

/**
 * A pair of planes to upload: `nt` rows of `nf` cells, row-major, earliest row first.
 *
 * Widened from [[TileData]] (T-1042) so the **live ring**'s buffers go up through the same two
 * `texSubImage2D` calls a tile's do. The alternative was a second uploader, and a second uploader is
 * a second set of texture parameters — the NEAREST/CLAMP pair that keeps a cell a measurement rather
 * than an interpolation is stated once, in [[GlTileTextures.plane]], and must stay that way.
 */
export interface PlaneSource {
  readonly nf: number;
  readonly nt: number;
  readonly value: Float32Array;
  readonly state: Uint8Array;
}

/** Uploads decoded tiles as an R16F measurement plane and an R8 state plane: 3 bytes a cell. */
export class GlTileTextures implements TileTextures<TilePlanes> {
  constructor(private readonly gl: GL) {}

  upload(data: PlaneSource): TilePlanes {
    const gl = this.gl;
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    const value = this.plane(gl.R16F, data.nf, data.nt);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, data.nf, data.nt, gl.RED, gl.FLOAT, data.value);
    const state = this.plane(gl.R8, data.nf, data.nt);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, data.nf, data.nt, gl.RED, gl.UNSIGNED_BYTE, data.state);
    return { value, state };
  }

  /** Rewrite rows `[row0, row0 + rows)` of both planes in place: a pushed row's upload (T-893). */
  patch(t: TilePlanes, data: PlaneSource, row0: number, rows: number): void {
    const gl = this.gl, nf = data.nf;
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);
    gl.bindTexture(gl.TEXTURE_2D, t.value);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, row0, nf, rows, gl.RED, gl.FLOAT, data.value.subarray(row0 * nf, (row0 + rows) * nf));
    gl.bindTexture(gl.TEXTURE_2D, t.state);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, row0, nf, rows, gl.RED, gl.UNSIGNED_BYTE, data.state.subarray(row0 * nf, (row0 + rows) * nf));
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

/** One pane's ring texture, and how far into the ring it has been uploaded (T-1042). */
interface RingTex {
  readonly planes: TilePlanes;
  readonly epoch: number;
  readonly nf: number;
  readonly capacity: number;
  /** The ring's `writes` count at the last upload: the rows since are what still has to go up. */
  uploaded: number;
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

  private lattices: LatticeSet;

  /** The tier each pane drew from on the previous frame, by pane id — the memory [[tierFor]]'s
   * hysteresis needs. Panes come and go, and a pane this map has never seen is `null`, which is
   * the plain budget rule; nothing here has to be cleaned up when one disappears beyond the
   * entry it leaves behind, which is one string and one enum. */
  private lastTier = new Map<string, ViewTier>();

  /**
   * **The coverage survey consulted before any tile is requested** (T-580, `./survey.ts`).
   *
   * `null` (the default) is *no survey*: every tile is fetched, as before T-580. `"awaiting"` is a
   * host that has a survey on the way — then nothing is requested at all until it lands, because
   * the whole point is to ask the coverage map FIRST; the pane shows its PENDING ground meanwhile,
   * which is true. A host whose survey fails sets `null` again and loses only the saving.
   */
  private survey: Survey | "awaiting" | null = null;
  /** One `UNOBSERVED` cell, uploaded once: what a surveyed place is drawn from, so its grey still
   * comes out of a coverage state byte and out of nothing else. */
  private greyTex: TilePlanes | null = null;

  /**
   * **The live ring, per pane** (T-1042 / LSR-1, `./livering.ts`): the published `spectrum/live`
   * rows a following pane paints its live edge from.
   *
   * `null` (the default) is the pre-LSR renderer, byte for byte: nothing is asked of the source, no
   * extent is excluded from the tile lane, and every pane is drawn from tiles alone. That is what the
   * feature flag switches — the host passes a source or it does not.
   */
  private rings: LiveRingSource | null = null;
  /** Each pane's ring texture, and how much of the ring has been uploaded into it. Keyed by pane, so
   * a pane that stops following (or closes) leaves one entry, dropped on the first frame it asks for
   * no ring. */
  private ringTex = new Map<string, RingTex>();
  /** The `t1Ns` of the newest ring row this pane last recorded a latency sample for (T-1048 /
   * LSR-7) — so a pane holding the same newest row across several frames (no new row has arrived)
   * records one sample per row, not one per frame. Cleared with the pane's ring texture. */
  private ringLatencySeen = new Map<string, number>();

  constructor(
    readonly canvas: HTMLCanvasElement,
    lattice: Lattice | LatticeSet,
    cache: TileCache<TilePlanes> | ((tex: TileTextures<TilePlanes>) => TileCache<TilePlanes>),
    opts: SurfaceOptions = {},
  ) {
    this.lattices = "detail" in lattice ? lattice : oneTier(lattice);
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
    for (const n of ["uRect", "uUv0", "uUv1", "uValue", "uState", "uKind", "uFlat", "uLo", "uHi", "uFallback", "uSizePx", "uTier", "uSrcPx", "uShadowGain", "uFog"]) {
      this.u[n] = gl.getUniformLocation(p, n);
    }
    this.vao = gl.createVertexArray()!;
    this.cache = typeof cache === "function" ? cache(new GlTileTextures(gl)) : cache;
    // Every lane that can start a request reads the survey, not only [[render]] (T-905).
    this.cache.setSettled((a) => this.settledBySurvey(a));
  }

  /** The **detail** lattice both axes are addressed on. Set once from a probe; changing it drops
   * nothing — the cache keys carry the scheme, so tiles of two lattices cannot be confused. */
  setLattice(lat: Lattice): void { this.lattices = { ...this.lattices, detail: lat }; }
  /** Both tiers at once (T-505). The overview lattice is a second probe, so a host that has not
   * got one yet sets only the detail lattice and every viewport stays on it. */
  setLattices(set: LatticeSet): void { this.lattices = set; }
  /** The detail lattice — the live edge's own, which is what an edge invalidation is about. */
  get lat(): Lattice { return this.lattices.detail; }
  get tiers(): LatticeSet { return this.lattices; }

  /** Hand the renderer a coverage survey, `"awaiting"` one, or `null` for none (T-580). */
  setSurvey(s: Survey | "awaiting" | null): void { this.survey = s; }
  get surveyState(): Survey | "awaiting" | null { return this.survey; }

  /**
   * **Where each pane's live rows come from** (T-1042), or `null` for none — which is the renderer's
   * default and the behaviour every existing test and every unflagged page keeps.
   *
   * The source is asked per pane per frame. Nothing here decides *which* panes have a ring: follow,
   * pause and the device are the host's (see [[LiveRingSource]]), exactly as the row-feed lane's
   * following-pane question is `SurfacePreview`'s.
   */
  setLiveRings(src: LiveRingSource | null): void {
    this.rings = src;
    if (!src) this.dropRings();
  }

  /**
   * **May no request be started for this place?** (T-905) — the cache's gate for every miss lane.
   * True while a survey is awaited (coverage FIRST: nothing is requested before it answers) and
   * where the survey settles the place as never sampled; false with no survey, and for an address
   * on a lattice this surface does not know (the conservative direction: fetch).
   */
  private settledBySurvey(a: TileAddr): boolean {
    const s = this.survey;
    if (s === null) return false;
    if (s === "awaiting") return true;
    const { detail, overview } = this.lattices;
    const lat = a.scheme === detail.scheme ? detail : a.scheme === overview.scheme ? overview : null;
    return lat !== null && s.unobservedThrough(extentOf(lat, a)) !== null;
  }

  /** When the survey settles `region` as never sampled, the instant it is grey up to; else null. */
  private surveyedThrough(region: Box): number | null {
    const s = this.survey;
    return s && s !== "awaiting" ? s.unobservedThrough(region) : null;
  }

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
    /** The panes drawn this frame, so a ring texture cannot outlive the pane it was made for
     * (T-1042): a closed split's would otherwise hold megabytes for a viewport that is gone. */
    const drawn = new Set<string>();
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
      drawn.add(pane.id);
      gl.viewport(r.x, r.y, r.w, r.h);
      gl.enable(gl.SCISSOR_TEST);
      gl.scissor(r.x, r.y, r.w, r.h);
      // The pane's ground is PENDING, not grey: before any tile arrives the honest statement is
      // "not loaded", and a grey clear would say "never observed" for the whole pane. The spike
      // cleared panes to grey; that is precisely finding F3, one line long.
      gl.clearColor(PENDING[0], PENDING[1], PENDING[2], 1);
      gl.clear(gl.COLOR_BUFFER_BIT);
      // T-807: the pane's coverage-fog layer, for every cell this pane draws (tiles, stand-ins and
      // surveyed places alike — they all reach the one `cellMark`).
      gl.uniform1i(this.u.uFog, pane.fog === false ? 0 : 1);

      // **Which tier answers this viewport** (T-505). Decided per pane, per frame, from the pane's
      // own box and rectangle — the same pass that lays out everything else, never a mode a host
      // sets. `lat` then stands for `this.lattices.detail` everywhere below, so a pane drawn from
      // the overview tier addresses, falls back, pins and cancels entirely inside that lattice.
      // The tier this pane drew from last frame, so the budget is a ceiling it crosses rather than
      // a line it sits on — see [[tierFor]] for what a flapping tier costs the tile route.
      const { tier, lat, levelF, levelT, addrs, clamped } =
        tierFor(this.lattices, pane.box, r.w, r.h, pane.device ?? "any", this.lastTier.get(pane.id) ?? null);
      this.lastTier.set(pane.id, tier);
      viewports.push({ box: pane.box, levelF, levelT, lat });
      let tiles = 0, fallbacks = 0, pending = 0, refused = 0, behind = 0, blank = 0, surveyed = 0;
      // T-916: the shadow's provenance, counted over the tiles this pane actually DREW (stand-ins
      // included — their cells are what is on the screen here), so the readout names a coarser
      // last-known source only when one is visible.
      let shadowLadder = 0, shadowCellHz = 0, shadowCellS = 0;
      const shadowOf = (d: TileData): void => {
        const src = d.shadowSource;
        if (!src || src.ladder + src.unstated === 0) return;
        shadowLadder++;
        if (src.coarsest) {
          shadowCellHz = Math.max(shadowCellHz, src.coarsest.fHz);
          shadowCellS = Math.max(shadowCellS, src.coarsest.tS);
        }
      };
      let drawnToNs = -Infinity;
      // **The live rows this pane paints its edge from** (T-1042), and the extent they make the tile
      // lane's business no longer. Asked, planned and discarded inside this frame, from this frame's
      // box — the same rule every other rectangle here obeys.
      const ring = this.rings?.ringFor(pane.id) ?? null;
      const plan = ring ? ringPlan(ring, pane.box, r.h) : null;
      let ringRows = 0, ringTiles = 0;
      const awaiting = this.survey === "awaiting";
      for (const a of addrs) {
        // **The ring's extent short-circuits the tile lane** (T-1042): everything this pane shows of
        // this address is already painted from rows that arrived before any tile for them could be
        // produced, so it is neither drawn nor *requested*. Before `isResident` and before
        // `acquire`, for the same reason T-580's survey check is: the cheapest answer is the one
        // that costs no request at all.
        if (plan?.cover && ringCovers(plan.cover, extentOf(lat, a), pane.box)) { ringTiles++; continue; }
        // **Ask the coverage map first** (T-580). A resident copy is always drawn — it is the finer
        // answer — but a place the survey settles as never sampled is not requested at all, and a
        // surface still waiting for its survey requests nothing yet.
        if (!this.cache.isResident(a)) {
          if (awaiting) { pending++; continue; }
          const region = extentOf(lat, a);
          const through = this.surveyedThrough(region);
          if (through !== null) {
            if (through > region.t0Ns) {
              const drawn = through >= region.t1Ns ? region : { ...region, t1Ns: through };
              this.drawSurveyed(pane, drawn, r);
              drawnToNs = Math.max(drawnToNs, drawn.t1Ns);
            }
            surveyed++;
            continue;
          }
        }
        const res = this.cache.acquire(a);
        if (res.kind === "resident") {
          const region = extentOf(lat, a);
          const shown = this.drawUpToHorizon(pane, lat, region, res.entry, "tile", r);
          if (shown.behind) behind++;
          if (shown.drawn) drawnToNs = Math.max(drawnToNs, shown.drawn.t1Ns); else blank++;
          // The cells of this tile that are inside this pane's box — the measurement the viewport
          // mode is a scale over. A resident tile draws its own extent, so the texture's extent and
          // the region are the same box — **clipped at the horizon** (T-532) when the answer stops
          // short, so the scale is measured over what was DRAWN and never over rows this copy does
          // not reach.
          if (shown.drawn) measure?.add(res.entry.data, region, shown.drawn, pane.box);
          if (shown.drawn) shadowOf(res.entry.data);
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
        const stand = this.fallbackFor(lat, a);
        // **Finer tiles already in hand stand in too** (T-893): a time zoom-out on a following pane
        // asks for a coarser level while the rows it was just drawing are resident one level down,
        // and searching only upwards left the pane blank until the coarser answer arrived. Drawn
        // after the ancestor, so where both exist the finer (and newer) rows are the ones seen.
        const finer = this.finerFor(lat, a);
        if (stand || finer.length) {
          const region = extentOf(lat, a);
          let drew = false, late = false;
          if (stand) {
            this.cache.standIn(stand);
            const shown = this.drawUpToHorizon(pane, lat, region, stand, "fallback", r);
            late ||= shown.behind;
            if (shown.drawn) { drew = true; drawnToNs = Math.max(drawnToNs, shown.drawn.t1Ns); }
            // A coarse stand-in's cells ARE what is on the screen here, so they count — but the
            // texture is the ancestor's, so the visible sub-rect is mapped through the ancestor's own
            // extent, not the child's. Getting that pair the wrong way round would read a different
            // corner of the ancestor than the one being displayed.
            if (shown.drawn) measure?.add(stand.data, extentOf(lat, stand.addr), shown.drawn, pane.box);
            if (shown.drawn) shadowOf(stand.data);
          }
          for (const c of finer) {
            this.cache.standIn(c);
            const own = extentOf(lat, c.addr);
            const shown = this.drawUpToHorizon(pane, lat, own, c, "fallback", r);
            late ||= shown.behind;
            if (shown.drawn) { drew = true; drawnToNs = Math.max(drawnToNs, shown.drawn.t1Ns); }
            if (shown.drawn) measure?.add(c.data, own, shown.drawn, pane.box);
            if (shown.drawn) shadowOf(c.data);
          }
          if (late) behind++;
          if (!drew) blank++;
          fallbacks++;
        }
        // **`pending` and `refused` are drawn apart** (T-499). A place with no usable answer is not
        // waiting for one — the route refused it, or the server is unreachable and this client is
        // backing off — and painting it as PENDING is a progress bar that never finishes. Neither
        // branch can produce grey: both are [[DrawKind]]s, and grey comes only from a state byte.
        else if (res.failed) { this.drawRefused(pane, extentOf(lat, a), r); refused++; }
        else { this.drawFlat(pane, extentOf(lat, a), PENDING, r); pending++; }
      }
      // §5.5's second pin, and a **coarse-first fill**. These go on the queue *after* the pane's
      // own tiles, and the queue is LIFO, so a parent is fetched FIRST — deliberately: at 11.4 ms a
      // tile one parent covers four children's worth of screen through the fallback path, so a cold
      // viewport shows something honest in a quarter of the time. It is also what makes a zoom-out
      // draw instead of flash.
      if (this.pinParents && !awaiting && levelF + 1 < lat.levelsF) {
        for (const a of tilesFor(lat, pane.box, levelF + 1, Math.min(levelT + 1, lat.levelsT - 1), pane.device ?? "any")) {
          // The same short-circuit as the pane's own tiles: a parent over never-sampled spectrum
          // is not worth a request either (T-580).
          if (!this.cache.isResident(a) && this.surveyedThrough(extentOf(lat, a)) !== null) continue;
          this.cache.prefetch(a);
        }
      }
      // **The rows, last** (T-1042): submitted after every tile, so where a ring row and a tile cell
      // describe the same instant the ROW is what is seen. That is the ordering the live-rendering
      // invariant asks for — the row exists, so it is on screen — and it is why a tile only partly
      // under the ring is still drawn: the part the rows do not reach keeps its measurement.
      let ringLatencyMs: number | null = null;
      if (ring && plan) {
        const planes = plan.draws.length ? this.ringPlanes(pane.id, ring) : null;
        if (planes) {
          for (const d of plan.draws) {
            this.drawRing(pane, ring, planes, d, r);
            ringRows += d.span.rows;
            drawnToNs = Math.max(drawnToNs, d.region.t1Ns);
          }
        }
        // T-1048 (LSR-7): **row t vs rAF**, one sample per NEW row rather than one per frame — the
        // pane's `cover` is the newest run clipped to its box, and its `t1Ns` only advances when a
        // fresh row actually arrived (`ringPlan`'s rule 2 in `./livering.ts`'s header). The latency
        // itself is this draw call's wall clock minus that row's own ARRIVAL wall clock
        // (`lastRowArrival`, set where the row reached the socket) — never the row's capture time,
        // which is not wall-clock-comparable on a replay (`./livemetrics.ts`'s header).
        if (plan.cover) {
          const seen = this.ringLatencySeen.get(pane.id);
          if (seen !== plan.cover.t1Ns) {
            this.ringLatencySeen.set(pane.id, plan.cover.t1Ns);
            const arrival = lastRowArrival.get();
            if (arrival !== null) {
              ringLatencyMs = paintLatencyMs(performance.now(), arrival);
              liveMetrics.latency.push(ringLatencyMs);
            }
          }
        }
      } else if (this.ringTex.has(pane.id)) {
        // The pane asked for no ring this frame (it froze, or the host withdrew the source): its
        // texture is memory held for a claim nobody is making any more.
        this.dropRing(pane.id);
      }
      const shortNs = Number.isFinite(drawnToNs) ? Math.max(0, pane.box.t1Ns - drawnToNs) : 0;
      reports.push({ id: pane.id, tier, lat, clamped, levelF, levelT, tiles, fallbacks, pending, refused, behind, blank, shortNs, surveyed, shadowLadder, shadowCellHz, shadowCellS, ringRows, ringTiles, ringRowPx: plan?.rowPx ?? 0, ringLatencyMs });
    }
    gl.disable(gl.SCISSOR_TEST);
    for (const id of this.ringTex.keys()) if (!drawn.has(id)) this.dropRing(id);
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
    this.cache.setViewports(this.lattices.detail, viewports);
    this.cache.endFrame();
    this.lastFrame = reports;
    return reports;
  }

  /** The nearest resident coarser tile containing `a`, or null. Peeks only: the ancestor search
   * must not enqueue a fetch at every level it tries, or one miss becomes `maxFallbackSteps²`. */
  private fallbackFor(lat: Lattice, a: TileAddr): TileEntry<TilePlanes> | null {
    for (const anc of ancestorsOf(lat, a, this.maxFallbackSteps)) {
      const e = this.cache.peek(anc, true);
      if (e) return e;
    }
    return null;
  }

  /**
   * Resident tiles one level FINER than `a` that lie inside it (T-893) — on the time axis first,
   * then frequency, then both; the first split with anything in hand wins. Peeks only, like
   * [[fallbackFor]]: a stand-in search must never enqueue a fetch.
   */
  private finerFor(lat: Lattice, a: TileAddr): TileEntry<TilePlanes>[] {
    for (const [df, dt] of [[0, 1], [1, 0], [1, 1]] as const) {
      if (a.levelF - df < 0 || a.levelT - dt < 0) continue;
      const out: TileEntry<TilePlanes>[] = [];
      for (let i = 0; i < 2 ** df; i++) {
        for (let j = 0; j < 2 ** dt; j++) {
          const e = this.cache.peek({
            ...a, levelF: a.levelF - df, levelT: a.levelT - dt,
            fIndex: a.fIndex * 2 ** df + i, tIndex: a.tIndex * 2 ** dt + j,
          }, true);
          if (e) out.push(e);
        }
      }
      if (out.length) return out;
    }
    return [];
  }

  /**
   * Draw `region` from `entry`, **but only as far forward as that answer's evidence reaches**
   * (T-532). Returns true when the answer stopped short and a strip was left undrawn.
   *
   * # The defect this exists to make impossible
   *
   * A tile cache keeps answers; the radio keeps recording. The route's coverage plane is written
   * from tune records, which stop at the newest sample, so a live tile's rows after that are served
   * `unobserved` — honest at the instant of the read, **false a moment later**. The copy is then
   * held for as long as the revalidation lane takes to come round (T-460/T-490/T-491), and every
   * row recorded in the meantime is drawn as THE grey: *the radio never looked here*, over rows the
   * radio recorded and the server is serving. That is the one claim this surface may never make by
   * accident.
   *
   * It was invisible while the finest time cell was one second, because the error hid inside the
   * cell the live edge was already in. At T-501's fidelity floor the cell is 40 ms and the same
   * staleness is a visible band across the newest second or two of every following pane.
   *
   * # Why nothing is drawn there rather than something else
   *
   * The pane's ground is already PENDING — *not loaded*, the honest statement for a place this
   * client has no answer for — and for this strip that is exactly true: the copy in hand does not
   * reach it. Drawing a mark of its own would be a seventh cell state for a condition that is a
   * property of the **answer**, not of the cell, and `cellrule.ts`'s standing rule is that
   * not-having-it is a tile property and never a cell state. So the strip falls through to the
   * ground, one comparison and no new vocabulary.
   *
   * A sealed tile is untouched: its extent ends before the horizon, so the whole of it is drawn.
   */
  private drawUpToHorizon(
    pane: PaneView, lat: Lattice, region: Box, entry: TileEntry<TilePlanes>, kind: DrawKind, rect: PaneRect,
  ): { behind: boolean; drawn: Box | null } {
    const asOf = entry.data.asOfNs;
    // No stated horizon is not "reaches everywhere": it is a band no record touches (or a server
    // that predates the field), and then the answer stands exactly as served.
    //
    // **Anything but a finite number is "no horizon", and that is deliberate** — the test named
    // *"a tile with NO `measured` renders"* is the standing rule that no missing input may blank a
    // pane, and a horizon read as `undefined` would blank every one of them.
    if (!Number.isFinite(asOf as number) || (asOf as number) >= region.t1Ns) {
      this.drawRegion(pane, lat, region, entry, kind, rect);
      return { behind: false, drawn: region };
    }
    if ((asOf as number) > region.t0Ns) {
      const drawn = { ...region, t1Ns: asOf as number };
      this.drawRegion(pane, lat, drawn, entry, kind, rect);
      return { behind: true, drawn };
    }
    return { behind: true, drawn: null };
  }

  /** Draws `region` of the surface from `entry`'s texture — the whole tile when they coincide, a
   * sub-rect when an ancestor is standing in for one of its children. */
  private drawRegion(pane: PaneView, lat: Lattice, region: Box, entry: TileEntry<TilePlanes>, kind: DrawKind, rect: PaneRect): void {
    const gl = this.gl;
    const tex = extentOf(lat, entry.addr);
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

  /**
   * **This pane's ring texture, caught up to the rows that have arrived** (T-1042).
   *
   * Per pane, and rebuilt whenever the ring's `epoch` moves — a retune ends a band, and patching
   * rows of the new one into the old one's texture would leave the two interleaved in the same
   * picture. Otherwise only the **rows appended since the last frame** are uploaded: two
   * `texSubImage2D` calls at most (one where the ring wrapped), of `nf` cells each, at the ~25–40
   * rows a second the stream publishes. Nothing re-uploads the whole ring per frame, which is the
   * cost that would make a ring worse than the tiles it replaces.
   */
  private ringPlanes(paneId: string, ring: RingFrame): TilePlanes {
    const tex = new GlTileTextures(this.gl);
    // The ring's buffers as the uploader reads them: `capacity` rows of `nf` cells.
    const planes: PlaneSource = { nf: ring.nf, nt: ring.capacity, value: ring.value, state: ring.state };
    let held = this.ringTex.get(paneId);
    if (held && (held.epoch !== ring.epoch || held.nf !== ring.nf || held.capacity !== ring.capacity)) {
      tex.destroy(held.planes);
      this.ringTex.delete(paneId);
      held = undefined;
    }
    if (!held) {
      held = {
        planes: tex.upload(planes), epoch: ring.epoch, nf: ring.nf, capacity: ring.capacity,
        uploaded: ring.writes,
      };
      this.ringTex.set(paneId, held);
      return held.planes;
    }
    // The rows this texture has not seen, newest-capacity at most: a page that was in a background
    // tab for a minute has had the whole ring rewritten under it, and patching a million rows to
    // arrive at the same bytes is work with no picture in it.
    const behind = Math.min(ring.capacity, ring.writes - held.uploaded);
    if (behind > 0) {
      const first = (ring.writes - behind) % ring.capacity;
      const runs: [number, number][] = first + behind <= ring.capacity
        ? [[first, behind]]
        : [[first, ring.capacity - first], [0, behind - (ring.capacity - first)]];
      for (const [row0, rows] of runs) tex.patch(held.planes, planes, row0, rows);
      held.uploaded = ring.writes;
    }
    return held.planes;
  }

  /**
   * One run of ring rows, over the region of the surface it was recorded across.
   *
   * Drawn through the **same program, ramp, display range and cell rule** as a tile — one `uValue`
   * sampler over dBFS/Hz, one `uState` sampler over the coverage codes — because "same measurement,
   * same colour" may not depend on which lane a row reached the screen by. The tier is `live-iq`:
   * these are the front end's own FFT rows, the finest thing this client can be shown, and saying so
   * is what keeps the three honesty tiers readable (docs/16 §8.3).
   *
   * `uSrcPx` is the on-screen size of one measured cell — one bin by one row — so the survey-overview
   * lattice mark, were a ring ever drawn coarser than its rows, would state the replication instead
   * of smoothing it. (It cannot be: `ringPlan` stands the ring aside below `MIN_ROW_PX`.)
   */
  private drawRing(pane: PaneView, ring: RingFrame, planes: TilePlanes, d: RingDraw, rect: PaneRect): void {
    const gl = this.gl;
    const clip = toClip(d.region, pane.box);
    const u0 = (d.region.f0Hz - ring.f0Hz) / (ring.f1Hz - ring.f0Hz);
    const u1 = (d.region.f1Hz - ring.f0Hz) / (ring.f1Hz - ring.f0Hz);
    // The run's rows, mapped by the run's own measured cadence: the row holding `t` is
    // `row0 + (t - t0) / (t1 - t0) * rows`, in texture rows, and `v` is that over the capacity.
    const dt = d.span.t1Ns - d.span.t0Ns;
    const row = (t: number) => d.span.row0 + ((t - d.span.t0Ns) / dt) * d.span.rows;
    const v0 = row(d.region.t0Ns) / ring.capacity;
    const v1 = row(d.region.t1Ns) / ring.capacity;
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, planes.value);
    gl.uniform1i(this.u.uValue, 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, planes.state);
    gl.uniform1i(this.u.uState, 1);
    gl.uniform1i(this.u.uKind, KIND_TILE);
    gl.uniform1f(this.u.uFallback, 0);
    gl.uniform1i(this.u.uTier, tierByte("live-iq"));
    gl.uniform4f(this.u.uRect, clip[0], clip[1], clip[2], clip[3]);
    gl.uniform2f(this.u.uUv0, u0, v0);
    gl.uniform2f(this.u.uUv1, u1, v1);
    const wPx = ((clip[2] - clip[0]) / 2) * rect.w, hPx = ((clip[3] - clip[1]) / 2) * rect.h;
    gl.uniform2f(this.u.uSizePx, wPx, hPx);
    // `nt` is the texture's rows and `dv` the fraction of it on screen, exactly as `drawRegion`
    // passes them, so this comes out as the height of one ring row in device pixels.
    const src = sourceCellPx(
      { nf: ring.nf, nt: ring.capacity, measured: { nf: ring.nf, nt: ring.capacity } },
      u1 - u0, v1 - v0, wPx, hPx,
    );
    gl.uniform2f(this.u.uSrcPx, src[0], src[1]);
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    this.drawCalls++;
  }

  /** Release one pane's ring texture. */
  private dropRing(paneId: string): void {
    const held = this.ringTex.get(paneId);
    if (held) new GlTileTextures(this.gl).destroy(held.planes);
    this.ringTex.delete(paneId);
    this.ringLatencySeen.delete(paneId);
  }

  /** Release every ring texture: the source was withdrawn, or the renderer is being disposed. */
  private dropRings(): void {
    for (const id of [...this.ringTex.keys()]) this.dropRing(id);
  }

  /**
   * A place the coverage survey settled as never sampled (T-580), drawn through the ordinary tile
   * path from ONE `UNOBSERVED` state byte — so grey here, as everywhere, comes out of the cell rule's
   * unobserved branch and out of no flat colour chosen in this file.
   */
  private drawSurveyed(pane: PaneView, region: Box, rect: PaneRect): void {
    const gl = this.gl;
    if (!this.greyTex) {
      // One cell, one state byte: everything else a tile carries is about a tile, and since T-1042
      // the uploader asks for the planes ([[PlaneSource]]) and nothing else.
      this.greyTex = new GlTileTextures(gl).upload({
        nf: 1, nt: 1, value: new Float32Array([NaN]), state: new Uint8Array([CELL.UNOBSERVED]),
      });
    }
    const clip = toClip(region, pane.box);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, this.greyTex.value);
    gl.uniform1i(this.u.uValue, 0);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, this.greyTex.state);
    gl.uniform1i(this.u.uState, 1);
    gl.uniform1i(this.u.uKind, KIND_TILE);
    gl.uniform1f(this.u.uFallback, 0);
    gl.uniform1i(this.u.uTier, tierByte("survey-overview"));
    gl.uniform4f(this.u.uRect, clip[0], clip[1], clip[2], clip[3]);
    gl.uniform2f(this.u.uUv0, 0, 0);
    gl.uniform2f(this.u.uUv1, 1, 1);
    const wPx = ((clip[2] - clip[0]) / 2) * rect.w, hPx = ((clip[3] - clip[1]) / 2) * rect.h;
    gl.uniform2f(this.u.uSizePx, wPx, hPx);
    gl.uniform2f(this.u.uSrcPx, Math.max(2, Math.abs(wPx)), Math.max(2, Math.abs(hPx)));
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
    if (this.greyTex) new GlTileTextures(this.gl).destroy(this.greyTex);
    this.greyTex = null;
    this.dropRings();
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
