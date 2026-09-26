// One tile, fetched from `GET /api/tiles` and decoded into the two planes the renderer samples
// (T-440, docs/api.md "GET /api/tiles").
//
// **Two planes, because the wire has two planes.** The route serves a measurement grid
// (`grid.max_db`) and a coverage grid (`coverage`), and its own rule is *"grey a cell of this tile
// if and only if the selected plane's state is `unobserved`"*. Packing both into one number would
// mean a state could be spelled as a value and a value as a state — the family of defect this
// milestone exists to end. So the measurement stays a float and the state stays a byte, and the
// only writer of the state plane is [[decodeTile]].
//
// **The coverage plane arrives run-length encoded, out of a table of distinct planes** (T-467).
// It used to arrive as one JSON object per cell carrying six sampling fields this client never
// read — 99 % of a 19.34 MB tile body, and duplicated, because `any` and `devices[0]` were the same
// plane on a one-device server. The states are `unobserved`,
// `observed` and `unknown` are separate codes in an alphabet the answer serves beside the runs, and
// a plane that does not decode exactly throws rather than resolving to any of them.
//
// **The measurement plane arrives as binary16, not as decimal text** (T-533). `grid.max_db` was
// 1 197 118 B of a 1 878 289 B live tile — seventeen significant digits per cell, for values this
// file writes straight into an R16F texture. T-1019 finished the job: every tile request carries
// `planes=compact` ([[TILE_PLANES]]), which types `frames` too and drops `grid.coverage` — the
// coverage plane beside the grid already carries it — and the answer states what it sent (`grid.encoding.planes`, and the plane's own
// type, byte order and transfer); an encoding this client does not know throws rather than being
// decoded as one it does.
//
// **A response we cannot read is not a coverage answer.** A malformed or truncated tile throws
// rather than decoding to `unobserved`: the place then stays *pending*, which is true, instead of
// claiming the radio never looked. The same reasoning as `BiasTee::Unknown` is not `Off`.

import { buildRequest, errorFrom } from "../controls/client";
import { CELL } from "./cellrule";
import { keyOf, latticeFrom, tileUrl, type Lattice, type TileAddr } from "./lattice";

/** Bytes one decoded cell occupies on the GPU: R16F measurement + R8 state. */
export const BYTES_PER_CELL = 3;

/** The honesty tier the route says answered. T-441 draws the three distinctly (see cellrule.ts). */
export type Tier = "live-iq" | "spectrum-history" | "survey-overview";

/**
 * A tier as a tile **states** it. `"unknown"` is what a tile built from pushed rows says when a block
 * it holds did not state a recognised tier (T-902): the honest answer is that we do not know, never
 * a tier borrowed from a neighbour. It draws with the weakest mark ([[tierByte]] fails closed).
 */
export type StatedTier = Tier | "unknown";

/** Weakest first is last: the order a tile holding several claims resolves them in (T-902). */
const TIER_ORDER: readonly StatedTier[] = ["live-iq", "spectrum-history", "survey-overview", "unknown"];

/** The weaker of two tier claims — a tile holding rows measured at two tiers states the weaker. */
export const weakerTier = (a: StatedTier, b: StatedTier): StatedTier =>
  TIER_ORDER.indexOf(a) >= TIER_ORDER.indexOf(b) ? a : b;

/** Is `x` one of the three tiers the routes state? */
export const isTier = (x: unknown): x is Tier => typeof x === "string" && TIERS.includes(x);

/** Per-axis fold direction, straight off `resolution.fold.<axis>.direction`. */
export type FoldDirection = "exact" | "folded" | "replicated";

/** A decoded tile: the two planes, plus what the route said about how it answered. */
export interface TileData {
  readonly addr: TileAddr;
  readonly key: string;
  readonly nf: number;
  readonly nt: number;
  /**
   * **`extent.t1_s`: where the route says this tile's time span ENDS, in ns** — or `null` when the
   * answer did not say (T-495).
   *
   * It is the discriminator between a tile that can still change and one that cannot, and it has to
   * be read from the answer rather than guessed. A tile whose own span is entirely behind the live
   * edge is finished and re-asking for it is waste; one whose span still reaches past the edge is
   * **being written right now**, and a copy of it taken earlier is missing every row recorded since.
   * Measured against a real server: a 32 s live tile fetched 2.5 s in answers `observed` for three
   * rows and **`unobserved` for the other twenty-nine** — THE grey — because at the instant of the
   * read the radio genuinely had not reached them. That grey is honest when it is served and becomes
   * a lie the moment capture continues, so what makes it a *permanent* lie is only ever failing to
   * ask again.
   *
   * `null` is not "sealed": [[TileCache]] falls back to the same number computed from the address,
   * which is where the route computes it from too. The answer is preferred because the answer is the
   * party that decided it.
   */
  readonly t1Ns: number | null;
  /**
   * **`coverage.horizon.as_of_s`: how far FORWARD this answer's coverage evidence reaches, in ns**
   * — or `null` when the answer named no horizon (no record touches this band, or a pre-T-532
   * server).
   *
   * It is the young-end twin of the route's `oldest_record_s`, and it exists because a tile cache
   * *keeps* answers. A tune record is written as capture proceeds, so it stops at the newest
   * sample; every row of a live tile after that is served `unobserved` — which is true at the
   * instant it is served and **false a moment later**, because those rows are being recorded while
   * this copy ages. Grey is the one mark that may only mean *the radio never looked*, so the
   * renderer draws nothing at all past this instant and leaves the pane's PENDING ground showing:
   * *this copy does not reach here* ([[Surface.drawUpToHorizon]]).
   *
   * `null` is **not** "reaches everywhere": with no horizon the answer stands as served, which is
   * right, because a band no record touches was never observed at any instant in it.
   */
  readonly asOfNs: number | null;
  /** Row-major `[t * nf + f]`, earliest row first, lowest frequency first — the route's own order.
   * `NaN` wherever the state plane does not say `OBSERVED`; never a sentinel that could be read as
   * a level. */
  readonly value: Float32Array;
  /** Row-major, same order. One of [[CELL]]'s codes. */
  readonly state: Uint8Array;
  readonly tier: StatedTier;
  /** The pyramid level that actually answered (`resolution.answered.level`), for the per-pane
   * "stated level" §8.5a requires instead of pretending two viewports agree. */
  readonly answeredLevel: number;
  readonly fold: { readonly frequency: FoldDirection; readonly time: FoldDirection };
  /**
   * How many cells along each axis the front end **actually measured** across this tile —
   * `min(fold.<axis>.source_cells, served)`, defaulting to the tile's own grid.
   *
   * On a `replicated` axis this is smaller than `nf`/`nt`, and the difference *is* the claim the
   * tile would otherwise make silently: 10 source time cells stretched across 256 served ones is
   * not 256 measurements. The renderer draws the survey-overview lattice at exactly this pitch, so
   * a replicated tile shows the resolution it has instead of a smooth upscale of it (docs/16 §4,
   * T-342's rule, T-411's failure).
   */
  readonly measured: { readonly nf: number; readonly nt: number };
  /** Observed range of this tile's own measurements, or null — never used as a colour scale here
   * (that is one shared range across every pane), only reported. */
  readonly rangeDb: { readonly lo: number; readonly hi: number } | null;
  /** What this tile costs the resident budget. */
  readonly bytes: number;
  /** `cost.in_flight_limit`: the server's cap, so the client can adopt it rather than guess. */
  readonly serverInFlightLimit: number | null;
  /**
   * `cost.in_flight_share`: **this client's** cap (T-630). The server-wide limit is what protects
   * ingest; the share is what this client may hold of it while other clients are asking, and it is
   * the number to operate at. Null from a server that states none (pre-T-630), where the limit is
   * the only number there is.
   */
  readonly serverInFlightShare: number | null;
  /**
   * `cost.in_flight_held`: how many of the route's producer slots **this client held** at the
   * instant this answer was written (T-959), or null from a server that states none.
   *
   * It is the fact the client cannot know for itself. An aborted read reaches the browser and not
   * `hk-api`, so the route goes on producing that tile and holding its slot; the client's charge
   * for it ([[TileCache.abandon]]) is the route's *measured mean*, and under load an overview read
   * outlives the mean by seconds. This is the route's own count, so the charge is corrected to it
   * — downwards when a read finished early, upwards when it is still out — rather than guessed.
   */
  readonly serverInFlightHeld?: number | null;
  /**
   * Does [[serverInFlightHeld]] include **this** answer's own read?
   *
   * It does for every answer that took a slot, and it does not for a hot-tile-cache hit
   * (`cost.served_from: "hot-tile-cache"`), which is served without one. The client subtracts the
   * reads it is still waiting for from the held count to learn how many abandoned ones the route is
   * still producing, and getting this wrong by one is the difference between charging a leftover
   * read and releasing it early.
   *
   * Optional only so a locally-built [[TileData]] (a synthesised row tile, a stand-in) need not
   * answer a question about a read that never happened; every decoded answer states it.
   */
  readonly serverHoldsThisRead?: boolean;
  /**
   * **Where this tile's last-known (shadow) values were read from** (T-916), or `null` when the
   * answer carries no shadow run from before the tile.
   *
   * A shadow run is a measurement carried into rows the radio was not looking at, and the *cell it
   * was measured over* decides whether it is the same number the band's last live row was drawn
   * with. Since T-911 the route searches the tile's **own** level first, so a band that departed
   * recently carries exactly that cell; a band that left longer ago than the own-level reach
   * (~256 blocks) falls back to the spectrum-history **ladder**, whose cells are coarser in both
   * axes, and a max-hold over a coarser box reads **hotter** — measured 10–15 dB on the departed
   * FM band, up to 26 dB in single columns (T-911).
   *
   * That is not a defect to hide, it is a different resolution, and this surface's rule is that a
   * pane states the level it was drawn at. So the counts travel to [[PaneStatus]] and the readout
   * names the coarser source when one answered. `unstated` is a run whose source the answer did not
   * label (a pre-T-911 server): counted with the ladder for the statement, because the claim that
   * says least is "this may be coarser than the row above it".
   *
   * **Optional, and absent is no statement at all**: a tile this client built itself from pushed
   * rows carries no shadow block to summarise, and a pane says nothing about a source no answer
   * named. `null` is the same absence for a decoded answer that carried no run from before it.
   */
  readonly shadowSource?: ShadowSource | null;
}

/** Per-tile summary of the `shadow` block's `sources` table (T-916). See [[TileData.shadowSource]]. */
export interface ShadowSource {
  /** Runs carried from BEFORE the tile — the ones a search found. Runs from the tile's own grid
   * (`src = 0`, the carry and the backward fill) are not counted: their cell is the tile's. */
  readonly carried: number;
  /** Of `carried`, those the route labelled `search: "own-level"` — the tile's own cell. */
  readonly ownLevel: number;
  /** Of `carried`, those labelled `search: "ladder"` — a coarser cell, which reads hotter. */
  readonly ladder: number;
  /** Of `carried`, those whose source entry labelled no search at all (pre-T-911 server). */
  readonly unstated: number;
  /** The coarsest ladder/unstated source cell behind those runs, `(Hz, s)`, or `null` when every
   * carried run came from the tile's own level. `0` on an axis the answer did not state. */
  readonly coarsest: { readonly fHz: number; readonly tS: number } | null;
}

/** The shape this client reads. Structural, and only the fields it actually uses. */
export interface TileResponse {
  key: { device: string; scheme: string | number; level_f: number; level_t: number; f_index: number; t_index: number; cells: number };
  extent: { nt: number; nf: number; t0_s?: number; t1_s?: number };
  axes: {
    frequency: { levels: number; cell_hz: number; max_level?: number };
    time: { levels: number; cell_s: number; max_level?: number };
  };
  grid: {
    nt: number; nf: number; max_db?: (number | null)[]; range_db?: { lo: number; hi: number } | null;
    /**
     * **How the per-cell planes are spelled** (T-533). Stated on every answer, including the JSON
     * one, so the encoding is read rather than inferred from which fields happen to be present.
     * `json` is the arrays below; `f16` moves `max_db` into [[grid.planes]].
     */
    encoding?: { planes?: string };
    /**
     * The typed spelling of the planes it names — `max_db` as base64 of little-endian IEEE
     * binary16. Every plane NOT named here is a JSON array beside it.
     */
    planes?: {
      max_db?: TilePlane;
      /** T-1019: the frame counts as unsigned integers, `u8`/`u16`/`u32`/`u64` wide. */
      frames?: TilePlane;
      occupancy_max?: TilePlane;
    };
    /** Per-cell folded frame count, in the JSON spelling. The evidence that separates
     * [[CELL.AWAITING]] from [[CELL.NO_LEVEL]] — see [[decodeTile]]. */
    frames?: (number | null)[];
    /** T-461: the one cell **every** cell of this grid is, served instead of the per-cell arrays
     * when the coverage map answered the tile on its own. `max_db: null` is the absence of a
     * level, never a level. */
    uniform?: { max_db: number | null; frames?: number | null };
  };
  coverage?: {
    grid?: { nt: number; nf: number };
    /** Code -> state name, served with the planes (T-467). A code is never read against an
     * alphabet the answer did not state. */
    states?: string[];
    /** Every **distinct** plane, once. `any` and each device name the one that is theirs, so a
     * one-device server no longer pays for two copies of the same plane. */
    planes?: { runs: number[]; cells: number; uniform?: string | null }[];
    any?: { plane: number };
    devices?: { device: string; plane: number }[];
    selected?: { device: string; named: boolean; present: boolean; plane?: number | null };
    /** T-532: `as_of_s` is how far forward this answer's records reach. See [[TileData.asOfNs]]. */
    horizon?: { as_of_s?: number | null };
  };
  resolution: {
    source: string;
    answered?: { level: number };
    fold?: {
      frequency?: { direction: string; source_cells?: number; served?: number };
      time?: { direction: string; source_cells?: number; served?: number };
    };
  };
  cost?: {
    in_flight_limit?: number; in_flight_share?: number; in_flight_held?: number;
    clients?: number; served_from?: string;
  };
  /**
   * **The last-known tier** (T-519/T-520, ADR-0020): column runs, each carrying a band's newest
   * known max-hold down rows the radio was not looking at. Parallel arrays of `runs` entries; run
   * `i` covers rows `[row[i], row[i] + rows[i])` of column `f[i]`, in the grid's own axes.
   * Absent (a pre-T-519 server) is no runs. Only what the renderer reads is typed.
   */
  shadow?: {
    encoding: string; runs: number;
    f: number[]; row: number[]; rows: number[]; last_db: number[]; last_t_s: number[]; src?: number[];
    /** The source table `src` indexes (T-911/T-916): entry 0 is this tile's own grid, the rest are
     * the store level a value before the tile came from, each naming the search that found it. */
    sources?: { from?: string; search?: string; level?: number; f_cell_hz?: number; t_cell_s?: number }[];
  } | null;
}

const TIERS: readonly string[] = ["live-iq", "spectrum-history", "survey-overview"];
const DIRECTIONS: readonly string[] = ["exact", "folded", "replicated"];

/** Thrown when the response is not a tile. The caller leaves the place *pending*, never grey. */
export class TileDecodeError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "TileDecodeError";
  }
}

/**
 * The route's ingest backpressure (`503`, naming its own cap), as a **first-class answer rather
 * than an error**: tile production takes the history lock, so the refusal means *ask again, fewer
 * at a time*, and the cap it names is the number to obey.
 */
export class TileBusyError extends Error {
  /**
   * @param limit the server-wide cap the refusal named, or null.
   * @param share **this client's** cap (T-630), or null from a server that named none. A refusal
   * is how a client learns its share shrank because another client arrived, so the number is
   * carried here and not only on the answers it is no longer getting.
   */
  /**
   * @param held what **this client** already holds of the route's slots, as the refusal states it
   * (T-959), or null from a server that states none. `held >= share` is the refusal a client caused
   * itself — its own reads, including the ones it walked away from and the route is still producing
   * — and it is not evidence about contention, so it must not halve the operating cap. `held <
   * share` is somebody else's slots, which is what AIMD's multiplicative decrease is for.
   */
  constructor(readonly limit: number | null, message: string, readonly share: number | null = null,
              readonly held: number | null = null) {
    super(message);
    this.name = "TileBusyError";
  }
}

/** The cap a `503` names ("too many tile reads in flight (limit 4, share 2)"), or null if it named none. */
export function capFromRefusal(message: string): number | null {
  return numberNamed(message, "limit");
}

/** This client's share, as a `503` names it (T-630), or null from a server that named none. */
export function shareFromRefusal(message: string): number | null {
  return numberNamed(message, "share");
}

/**
 * What this client holds, as a `503` names it (T-959) — **zero included**, because "you hold none
 * of these slots" is the reading that means the refusal is contention and the client should back
 * off. Null only when the server named no such number at all.
 */
export function heldFromRefusal(message: string): number | null {
  const m = /held\s+(\d+)/.exec(message);
  const n = m ? Number(m[1]) : NaN;
  return Number.isFinite(n) && n >= 0 ? n : null;
}

function numberNamed(message: string, word: string): number | null {
  const m = new RegExp(`${word}\\s+(\\d+)`).exec(message);
  const n = m ? Number(m[1]) : NaN;
  return Number.isFinite(n) && n > 0 ? n : null;
}

/**
 * Decodes one response into the two planes.
 *
 * The state plane is built from `coverage` — the route's own grey authority — and the measurement
 * only decides *within* an observed cell. That ordering matters: a `null` in `max_db` means the
 * pyramid holds no level here, which is a different statement from "nothing looked", and collapsing
 * the two is exactly the lie §4 forbids.
 *
 * **Observed with no level splits in two**, on evidence the wire already carries (T-441):
 *
 * | `coverage` | `grid.max_db` | `grid.frames` | state | the claim |
 * |---|---|---|---|---|
 * | `observed` | a number | – | `OBSERVED` | a measurement |
 * | `observed` | `null` | `0` | `AWAITING` | we looked; **nothing has been folded here** |
 * | `observed` | `null` | `> 0`, or absent | `NO_LEVEL` | we looked; no level is in hand |
 * | `unobserved` | – | – | `SHADOW` if a `shadow` run covers it (T-520), else `UNOBSERVED` | last-known, or THE grey |
 *
 * `frames == 0` is the whole discriminator, and it needs neither a clock nor a prediction — which
 * matters, because the alternative ("is this cell near the live edge?") is a guess about the
 * future, and the one thing the fifth state must not do is promise arrival. T-446's defect wrote
 * exactly this cell for an hour of capture time and never filled it.
 *
 * **The default is the claim that says least.** A tile without per-cell frame counts decodes to
 * `NO_LEVEL`, never `AWAITING`: the more specific state is granted only on positive evidence, the
 * same direction as `BiasTee::Unknown` is not `Off`.
 */
/** One typed plane out of `grid.planes`: the wire states its own type, order and transfer. */
interface TilePlane { type?: string; byte_order?: string; transfer?: string; cells?: number; data?: string }

export function decodeTile(addr: TileAddr, resp: TileResponse): TileData {
  const nf = resp.extent?.nf ?? resp.grid?.nf, nt = resp.extent?.nt ?? resp.grid?.nt;
  if (!(nf > 0) || !(nt > 0)) throw new TileDecodeError(`tile ${keyOf(addr)}: no grid dimensions`);
  const n = nf * nt;
  const db = resp.grid?.max_db;
  const frames = resp.grid?.frames;
  const uni = resp.grid?.uniform;
  // **Two spellings of one grid** (T-461). Per-cell arrays, or — when the coverage map answered the
  // tile on its own and every cell holds the same nothing — the one cell they would all have held.
  // Neither is a default for the other: a grid that is neither an array of the right length nor a
  // stated uniform cell is unreadable, and unreadable is not an answer.
  let levelAt: (i: number) => number | null | undefined;
  let framesAt: (i: number) => number | null | undefined;
  const packed = packedLevels(addr, resp, n);
  if (packed) {
    // T-1019: `frames` may be a typed plane beside the levels, or still a JSON array (a `f16`
    // server, or one that predates `compact`). Neither is a default for the other, and a plane
    // that is present and unreadable throws rather than decoding as "no counts".
    const packedCounts = packedFrames(addr, resp, n);
    const counted = Array.isArray(frames) && frames.length === n ? frames : null;
    levelAt = (i) => packed[i];
    framesAt = packedCounts ? (i) => packedCounts[i] : counted ? (i) => counted[i] : () => undefined;
  } else if (Array.isArray(db) && db.length === n) {
    const counted = Array.isArray(frames) && frames.length === n ? frames : null;
    levelAt = (i) => db[i];
    framesAt = counted ? (i) => counted[i] : () => undefined;
  } else if (uni && typeof uni === "object" && "max_db" in uni) {
    levelAt = () => uni.max_db;
    framesAt = () => uni.frames;
  } else {
    throw new TileDecodeError(`tile ${keyOf(addr)}: grid.max_db has ${db?.length ?? "no"} cells, no grid.planes.max_db and no grid.uniform, expected ${n}`);
  }
  const cov = coverageCells(addr, resp);
  const shade = shadowCells(addr, resp, nf, nt, levelAt);
  const value = new Float32Array(n);
  const state = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    const s = cov(i);
    if (s === "unobserved") {
      // THE grey, unless a real earlier measurement is carried here: then the last-known tier. The
      // shadow is consulted ONLY on this branch — an observed or unknown cell keeps its own mark
      // whatever the shadow plane says, because coverage alone decides grey (docs/api.md `shadow`).
      const sv = shade ? shade[i] : NaN;
      if (Number.isFinite(sv)) { state[i] = CELL.SHADOW; value[i] = sv; } else { state[i] = CELL.UNOBSERVED; value[i] = NaN; }
      continue;
    }
    if (s === "unknown") { state[i] = CELL.UNKNOWN; value[i] = NaN; continue; }
    const v = levelAt(i);
    // **`"excluded"` is an observation** (T-595): the radio sampled this cell and the analysis was
    // deliberately not run on it (the DC/LO notch). The level is real and is drawn — on the ramp,
    // with the exclusion inked over it — so the honest mark needs a level; with none in hand the
    // cell falls through to the marks below, which claim less. An UNKNOWN state name also falls
    // through here and draws its measurement rather than grey: a client that has not learned a new
    // word must never invent "nothing looked" out of it.
    if (typeof v === "number" && Number.isFinite(v)) {
      state[i] = s === "excluded" ? CELL.EXCLUDED : CELL.OBSERVED;
      value[i] = v;
      continue;
    }
    value[i] = NaN;
    state[i] = framesAt(i) === 0 ? CELL.AWAITING : CELL.NO_LEVEL;
  }
  const src = String(resp.resolution?.source ?? "");
  if (!TIERS.includes(src)) throw new TileDecodeError(`tile ${keyOf(addr)}: unknown honesty tier ${src || "(none)"}`);
  const dir = (d: unknown): FoldDirection => (DIRECTIONS.includes(String(d)) ? (String(d) as FoldDirection) : "exact");
  // `source_cells` is how many cells the fold READ, `served` how many it wrote. A replicated axis
  // read fewer than it served, and that count is the tile's real resolution on that axis. Absent or
  // nonsensical, the tile's own grid stands — claiming *less* measured detail than we can show
  // would be its own kind of invention.
  const measured = (f: { source_cells?: number; served?: number } | undefined, cells: number): number => {
    const src = f?.source_cells;
    return typeof src === "number" && Number.isFinite(src) && src > 0 ? Math.min(Math.round(src), cells) : cells;
  };
  return {
    addr,
    key: keyOf(addr),
    nf,
    nt,
    // Seconds on the wire, ns everywhere in this client. A response that omits it, or states
    // something unreadable, says nothing — and nothing said is not "sealed" (see [[TileData.t1Ns]]).
    t1Ns: Number.isFinite(resp.extent?.t1_s) ? Math.round((resp.extent!.t1_s as number) * 1e9) : null,
    // Same reading as `t1Ns`: the answer's own number, in ns, or `null` when it stated none. A
    // server that does not carry the field, or carries something unreadable, says nothing — and
    // nothing said leaves the answer standing as served (see [[TileData.asOfNs]]).
    asOfNs: Number.isFinite(resp.coverage?.horizon?.as_of_s)
      ? Math.round((resp.coverage!.horizon!.as_of_s as number) * 1e9)
      : null,
    value,
    state,
    tier: src as Tier,
    answeredLevel: Number(resp.resolution?.answered?.level ?? -1),
    fold: { frequency: dir(resp.resolution?.fold?.frequency?.direction), time: dir(resp.resolution?.fold?.time?.direction) },
    measured: {
      nf: measured(resp.resolution?.fold?.frequency, nf),
      nt: measured(resp.resolution?.fold?.time, nt),
    },
    rangeDb: resp.grid?.range_db ?? null,
    bytes: n * BYTES_PER_CELL,
    serverInFlightLimit: typeof resp.cost?.in_flight_limit === "number" ? resp.cost.in_flight_limit : null,
    serverInFlightShare: typeof resp.cost?.in_flight_share === "number" ? resp.cost.in_flight_share : null,
    serverInFlightHeld: typeof resp.cost?.in_flight_held === "number" ? resp.cost.in_flight_held : null,
    serverHoldsThisRead: resp.cost?.served_from !== "hot-tile-cache",
    shadowSource: shadowSourceOf(resp),
  };
}

/**
 * **Which search answered this tile's shadow runs** (T-916), from the `sources` table the route
 * serves beside them — or `null` when no run was carried in from before the tile.
 *
 * Read defensively and *downwards*: a run whose `src` index is missing, out of the table, or whose
 * entry names no `search` counts as `unstated`, and `unstated` is stated to the user in the same
 * breath as `ladder`. The claim that says least about a carried value is "this may have been
 * measured over a coarser cell than the row above it", and that is the honest default for an answer
 * that did not say. Nothing here throws: the source table is a *statement about* the shadow, not
 * the shadow, so an unreadable one must not take a legitimate last-known plane off the screen —
 * [[shadowCells]] still holds the plane itself to every rule it had.
 */
function shadowSourceOf(resp: TileResponse): ShadowSource | null {
  const sh = resp.shadow;
  if (!sh || typeof sh !== "object" || !Number.isInteger(sh.runs) || sh.runs <= 0) return null;
  const table = Array.isArray(sh.sources) ? sh.sources : [];
  const src = Array.isArray(sh.src) ? sh.src : null;
  let carried = 0, ownLevel = 0, ladder = 0, unstated = 0;
  let fHz = 0, tS = 0, coarse = false;
  for (let r = 0; r < sh.runs; r++) {
    // No `src` array at all: the run's provenance is unknown, not "this tile's own grid" — but a
    // stated 0 IS the tile's own grid, and carries no claim about a coarser cell.
    const i = src ? src[r] : NaN;
    if (i === 0) continue;
    carried++;
    const e = Number.isInteger(i) ? table[i as number] : undefined;
    const search = e && typeof e.search === "string" ? e.search : null;
    if (search === "own-level") { ownLevel++; continue; }
    if (search === "ladder") ladder++; else unstated++;
    coarse = true;
    if (e) {
      if (typeof e.f_cell_hz === "number" && Number.isFinite(e.f_cell_hz)) fHz = Math.max(fHz, e.f_cell_hz);
      if (typeof e.t_cell_s === "number" && Number.isFinite(e.t_cell_s)) tS = Math.max(tS, e.t_cell_s);
    }
  }
  if (carried === 0) return null;
  return { carried, ownLevel, ladder, unstated, coarsest: coarse ? { fHz, tS } : null };
}

/** Plane spellings this client can read. Anything else is refused, never guessed at (T-533). */
const PLANE_ENCODINGS: readonly string[] = ["json", "f16", "compact"];

/** The spellings that carry typed planes in `grid.planes` rather than JSON arrays. */
const PACKED_ENCODINGS: readonly string[] = ["f16", "compact"];

/**
 * The `max_db` plane out of `grid.planes`, decoded — or `null` when this answer spells it as a
 * JSON array (or as a uniform cell) instead (T-533).
 *
 * **The encoding is read from the answer, and an unrecognised one throws.** A plane decoded against
 * the wrong type is not a degraded measurement, it is a different number entirely, so the rule here
 * is the same one `coverage` follows: a response we cannot read is not an answer, and the place
 * stays *pending* rather than being painted with whatever the bytes happened to mean. Everything
 * checked rather than trusted: the declared type, byte order and transfer, and a `data` string that
 * decodes to exactly `2 · cells` bytes for this tile's own grid.
 *
 * `NaN` is *not observed*, the same claim `null` makes in the JSON spelling — never a level.
 */
function packedLevels(addr: TileAddr, resp: TileResponse, n: number): Float32Array | null {
  const named = resp.grid?.encoding?.planes;
  if (named !== undefined && !PLANE_ENCODINGS.includes(String(named))) {
    throw new TileDecodeError(`tile ${keyOf(addr)}: grid.encoding.planes is ${String(named)}, which this client cannot decode`);
  }
  const bin = planeBytes(addr, resp, "max_db", n, { f16: 2 });
  if (!bin) return null;
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    out[i] = f16ToF32(bin.data.charCodeAt(i * 2) | (bin.data.charCodeAt(i * 2 + 1) << 8));
  }
  return out;
}

/**
 * The `frames` plane out of `grid.planes`, decoded — or `null` when this answer spells the counts
 * as a JSON array instead (T-1019).
 *
 * **The width is the server's, read off the plane's own `type`.** `frames` is a count, so the
 * route picks the narrowest unsigned width that holds this tile's values rather than a fixed one;
 * the only thing this client asks of the number is `=== 0` ([[decodeTile]]'s `AWAITING`
 * discriminator), and a count above 2^53 is not representable here — which cannot arise from a
 * tile of folded frames and would in any case read as "many", never as zero.
 */
function packedFrames(addr: TileAddr, resp: TileResponse, n: number): Float64Array | null {
  const bin = planeBytes(addr, resp, "frames", n, { u8: 1, u16: 2, u32: 4, u64: 8 });
  if (!bin) return null;
  const w = bin.width;
  const out = new Float64Array(n);
  for (let i = 0; i < n; i++) {
    let v = 0;
    for (let b = w - 1; b >= 0; b--) v = v * 256 + bin.data.charCodeAt(i * w + b);
    out[i] = v;
  }
  return out;
}

/**
 * One typed plane's bytes, with everything about it CHECKED rather than trusted: the answer's
 * stated encoding, the plane's own type against the widths this reader accepts, byte order,
 * transfer, cell count, and a `data` string that decodes to exactly `width · cells` bytes.
 *
 * **A plane decoded against the wrong type is not a degraded measurement, it is a different number
 * entirely**, so anything unreadable throws and the place stays *pending* rather than being
 * painted with whatever the bytes happened to mean. Absent is `null` — the JSON spelling, which
 * every caller here already handles.
 */
function planeBytes(
  addr: TileAddr, resp: TileResponse, name: "max_db" | "frames", n: number,
  widths: Record<string, number>,
): { data: string; width: number } | null {
  const named = resp.grid?.encoding?.planes;
  const p = resp.grid?.planes?.[name];
  if (p === undefined || p === null) return null;
  const bad = (why: string) => new TileDecodeError(`tile ${keyOf(addr)}: grid.planes.${name} ${why}`);
  if (!PACKED_ENCODINGS.includes(String(named))) throw bad(`is present but grid.encoding.planes says ${String(named)}`);
  const width = widths[String(p.type)];
  if (width === undefined) throw bad(`has type ${String(p.type)}, not one of ${Object.keys(widths).join(", ")}`);
  if (p.byte_order !== "little-endian") throw bad(`has byte order ${String(p.byte_order)}`);
  if (p.transfer !== "base64") throw bad(`has transfer ${String(p.transfer)}`);
  if (p.cells !== n) throw bad(`covers ${String(p.cells)} cells, expected ${n}`);
  if (typeof p.data !== "string") throw bad("carries no data");
  let bin: string;
  try {
    bin = atob(p.data);
  } catch {
    throw bad("is not base64");
  }
  if (bin.length !== n * width) throw bad(`decodes to ${bin.length} bytes, expected ${n * width}`);
  return { data: bin, width };
}

/** One IEEE 754 binary16, as the sixteen bits the wire sent. NaN and infinities stay non-finite. */
export function f16ToF32(bits: number): number {
  const sign = bits & 0x8000 ? -1 : 1, exp = (bits >> 10) & 0x1f, mant = bits & 0x3ff;
  if (exp === 0) return sign * mant * 2 ** -24;          // zero and subnormals
  if (exp === 31) return mant ? NaN : sign * Infinity;   // absent, or out of range
  return sign * (mant + 1024) * 2 ** (exp - 25);
}

/**
 * Rasterises `shadow`'s column runs into a per-cell last-known level (`NaN` = none), or `null`
 * when the answer carries no shadow (T-520).
 *
 * **The rules**, all from docs/api.md's `shadow` section, and each one checked rather than trusted:
 *
 *  - Absent or `null` is *no runs*: every `unobserved` cell stays grey, exactly as before T-519.
 *  - An encoding this client does not know, arrays that are not all `runs` long, a run outside the
 *    grid, a non-finite level or time, or **two runs over one cell** throw [[TileDecodeError]].
 *    An unreadable shadow is not "no shadow": decoding it as absent would paint grey — *no retained
 *    measurement reaches here* — over cells the server just said one does. The place stays
 *    *pending*, which is true.
 *  - **A run over a cell whose grid holds a measurement throws too.** The route guarantees a shadow
 *    never replaces a measurement; a response that breaks that has confused *which* number is the
 *    measurement of this cell, and the client does not get to pick one.
 *  - The caller applies a value **only where coverage says `unobserved`** — see [[decodeTile]].
 */
function shadowCells(
  addr: TileAddr, resp: TileResponse, nf: number, nt: number, levelAt: (i: number) => number | null | undefined,
): Float32Array | null {
  const sh = resp.shadow;
  if (sh === undefined || sh === null) return null;
  const bad = (why: string) => new TileDecodeError(`tile ${keyOf(addr)}: shadow ${why}`);
  if (typeof sh !== "object") throw bad("is not an object");
  if (sh.encoding !== "column-runs") throw bad(`encoding ${String(sh.encoding)} is not column-runs`);
  const runs = sh.runs;
  if (!Number.isInteger(runs) || runs < 0) throw bad(`runs ${String(runs)} is not a count`);
  if (runs === 0) return null;
  for (const k of ["f", "row", "rows", "last_db", "last_t_s"] as const) {
    if (!Array.isArray(sh[k]) || sh[k].length !== runs) throw bad(`${k} is not ${runs} entries`);
  }
  const out = new Float32Array(nf * nt).fill(NaN);
  for (let r = 0; r < runs; r++) {
    const f = sh.f[r], row = sh.row[r], rows = sh.rows[r], db = sh.last_db[r], t = sh.last_t_s[r];
    if (!Number.isInteger(f) || f < 0 || f >= nf) throw bad(`run ${r} column ${f} is outside ${nf}`);
    if (!Number.isInteger(row) || !Number.isInteger(rows) || row < 0 || rows < 1 || row + rows > nt) {
      throw bad(`run ${r} rows [${row}, ${row + rows}) are outside ${nt}`);
    }
    // The value AND when it was last true travel together (ADR-0020 §1); a level with no age is not
    // the last-known tier, it is an unlabelled number.
    if (typeof db !== "number" || !Number.isFinite(db)) throw bad(`run ${r} last_db is not a level`);
    if (typeof t !== "number" || !Number.isFinite(t)) throw bad(`run ${r} last_t_s is not a time`);
    for (let y = row; y < row + rows; y++) {
      const i = y * nf + f;
      if (!Number.isNaN(out[i])) throw bad(`runs overlap at column ${f} row ${y}`);
      const v = levelAt(i);
      if (typeof v === "number" && Number.isFinite(v)) throw bad(`run ${r} covers a measured cell (column ${f} row ${y})`);
      out[i] = db;
    }
  }
  return out;
}

/**
 * Expands one run-length-encoded plane into a state name per cell (T-467).
 *
 * The wire carries `runs` as a flat `[code, count, code, count, …]` and `states` as the alphabet
 * those codes index. **Both come from the same answer**, so a code is never resolved against an
 * alphabet this client assumed — and anything that does not add up (an odd run list, a code outside
 * the alphabet, a total that is not the plane's cell count) throws rather than being patched to a
 * state. A plane we cannot read is not a coverage answer: the place stays *pending*, never grey,
 * and never `observed`.
 */
export function expandPlane(addr: TileAddr, states: string[], runs: number[], cells: number): string[] {
  if (!Array.isArray(runs) || runs.length % 2 !== 0) {
    throw new TileDecodeError(`tile ${keyOf(addr)}: coverage runs are not [code, count] pairs`);
  }
  const out = new Array<string>(cells);
  let at = 0;
  for (let r = 0; r < runs.length; r += 2) {
    const code = runs[r], n = runs[r + 1];
    const name = states[code];
    if (typeof name !== "string") {
      throw new TileDecodeError(`tile ${keyOf(addr)}: coverage run code ${code} is not in the served alphabet`);
    }
    if (!Number.isInteger(n) || n < 0 || at + n > cells) {
      throw new TileDecodeError(`tile ${keyOf(addr)}: coverage run length ${n} overruns ${cells} cells`);
    }
    out.fill(name, at, at + n);
    at += n;
  }
  if (at !== cells) {
    throw new TileDecodeError(`tile ${keyOf(addr)}: coverage runs cover ${at} cells, expected ${cells}`);
  }
  return out;
}

/**
 * A reader for the selected device's coverage state at a tile-grid index, resampling nearest when
 * the coverage plane is not cell-for-cell aligned with the tile (`coverage.grid.aligned`).
 *
 * The selection rule is the route's own and is applied here rather than taken from
 * `selected.plane`, because it is the one place the client must be sure of: a **named** device gets
 * that front end's own plane and *never* the union, or a merged plane would wear one radio's
 * identity (T-259/T-305).
 *
 * A **named** device with no plane in the answer is `unobserved` for that device — `selected.present
 * = false` is a coverage answer (that front end recorded nothing here), which the route says in so
 * many words. A *missing or malformed* coverage block is not, and throws.
 */
function coverageCells(addr: TileAddr, resp: TileResponse): (i: number) => string {
  const nf = resp.extent?.nf ?? resp.grid.nf, nt = resp.extent?.nt ?? resp.grid.nt;
  const c = resp.coverage;
  if (!c) throw new TileDecodeError(`tile ${keyOf(addr)}: no coverage plane, so no grey authority`);
  const named = addr.device !== "any";
  const index = named ? c.devices?.find((d) => d.device === addr.device)?.plane : c.any?.plane;
  const encoded = typeof index === "number" ? c.planes?.[index] : undefined;
  if (!encoded) {
    if (named && c.selected?.present === false) return () => "unobserved";
    throw new TileDecodeError(`tile ${keyOf(addr)}: coverage has no plane for device ${addr.device}`);
  }
  if (!Array.isArray(c.states) || c.states.length === 0) {
    throw new TileDecodeError(`tile ${keyOf(addr)}: coverage carries no state alphabet`);
  }
  const cnf = c.grid?.nf ?? nf, cnt = c.grid?.nt ?? nt;
  if (encoded.cells !== cnf * cnt) {
    throw new TileDecodeError(`tile ${keyOf(addr)}: coverage plane has ${encoded.cells} cells, expected ${cnf * cnt}`);
  }
  const plane = expandPlane(addr, c.states, encoded.runs, encoded.cells);
  if (cnf === nf && cnt === nt) return (i) => plane[i];
  // Nearest-cell resample. Only ever a downscale or an exact upscale of the same extent, so it
  // moves no boundary: both grids cover this tile and nothing else.
  return (i) => {
    const t = Math.min(cnt - 1, Math.floor((Math.floor(i / nf) * cnt) / nt));
    const f = Math.min(cnf - 1, Math.floor(((i % nf) * cnf) / nf));
    return plane[t * cnf + f];
  };
}

/** How the cache asks for a tile. `signal` lets a viewport change abandon one in flight. */
export type TileFetch = (url: string, init: RequestInit) => Promise<{ ok: boolean; status: number; statusText: string; json(): Promise<unknown> }>;

/**
 * Fetches and decodes one tile. `503` becomes [[TileBusyError]] carrying the cap the server named;
 * anything else non-2xx becomes the API's own `ControlError`.
 */
export async function fetchTile(addr: TileAddr, token: string, fetchFn: TileFetch, signal?: AbortSignal): Promise<TileData> {
  const req = buildRequest("GET", tileUrl(addr), token);
  const r = await fetchFn(req.url, { ...req.init, signal });
  const body = await r.json().catch(() => ({}));
  if (!r.ok) {
    const e = errorFrom(r.status, body, r.statusText);
    if (r.status === 503) {
      throw new TileBusyError(capFromRefusal(e.message), e.message, shareFromRefusal(e.message),
        heldFromRefusal(e.message));
    }
    throw e;
  }
  return decodeTile(addr, body as TileResponse);
}

/** The lattice bootstrap: one cheap tile (`cells=8`) whose `axes` state the ladder both axes run on. */
export function probeAddr(device = "any", scheme = "view"): TileAddr {
  return { device, scheme, levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 8 };
}

/**
 * The lattice, read off a probe response, at the `cells` the view will render at.
 *
 * **One implementation, in `lattice.ts`.** This was a second copy of [[latticeFrom]]'s arithmetic,
 * differing only in taking `cells` from the caller instead of the response — which is exactly the
 * drift shape `cellrule.ts` was rewritten to avoid, and it is why the route's `max_level` reached
 * one reader and not the other (T-480). It delegates now.
 */
export function latticeOf(resp: TileResponse, cells: number): Lattice {
  return latticeFrom(resp, cells);
}
