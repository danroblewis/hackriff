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
// plane on a one-device server. The states themselves are unchanged and still three: `unobserved`,
// `observed` and `unknown` are separate codes in an alphabet the answer serves beside the runs, and
// a plane that does not decode exactly throws rather than resolving to any of them.
//
// **A response we cannot read is not a coverage answer.** A malformed or truncated tile throws
// rather than decoding to `unobserved`: the place then stays *pending*, which is true, instead of
// claiming the radio never looked. The same reasoning as `BiasTee::Unknown` is not `Off`.

import { buildRequest, errorFrom } from "../controls/client";
import { CELL } from "./cellrule";
import { keyOf, tileUrl, type Lattice, type TileAddr } from "./lattice";

/** Bytes one decoded cell occupies on the GPU: R16F measurement + R8 state. */
export const BYTES_PER_CELL = 3;

/** The honesty tier the route says answered. T-441 draws the three distinctly (see cellrule.ts). */
export type Tier = "live-iq" | "spectrum-history" | "survey-overview";

/** Per-axis fold direction, straight off `resolution.fold.<axis>.direction`. */
export type FoldDirection = "exact" | "folded" | "replicated";

/** A decoded tile: the two planes, plus what the route said about how it answered. */
export interface TileData {
  readonly addr: TileAddr;
  readonly key: string;
  readonly nf: number;
  readonly nt: number;
  /** Row-major `[t * nf + f]`, earliest row first, lowest frequency first — the route's own order.
   * `NaN` wherever the state plane does not say `OBSERVED`; never a sentinel that could be read as
   * a level. */
  readonly value: Float32Array;
  /** Row-major, same order. One of [[CELL]]'s codes. */
  readonly state: Uint8Array;
  readonly tier: Tier;
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
}

/** The shape this client reads. Structural, and only the fields it actually uses. */
export interface TileResponse {
  key: { device: string; scheme: string | number; level_f: number; level_t: number; f_index: number; t_index: number; cells: number };
  extent: { nt: number; nf: number };
  axes: { frequency: { levels: number; cell_hz: number }; time: { levels: number; cell_s: number } };
  grid: {
    nt: number; nf: number; max_db: (number | null)[]; range_db?: { lo: number; hi: number } | null;
    /** Per-cell folded frame count. The evidence that separates [[CELL.AWAITING]] from
     * [[CELL.NO_LEVEL]] — see [[decodeTile]]. */
    frames?: (number | null)[];
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
  };
  resolution: {
    source: string;
    answered?: { level: number };
    fold?: {
      frequency?: { direction: string; source_cells?: number; served?: number };
      time?: { direction: string; source_cells?: number; served?: number };
    };
  };
  cost?: { in_flight_limit?: number };
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
  constructor(readonly limit: number | null, message: string) {
    super(message);
    this.name = "TileBusyError";
  }
}

/** The cap a `503` names ("too many tile reads in flight (limit 4)"), or null if it named none. */
export function capFromRefusal(message: string): number | null {
  const m = /limit\s+(\d+)/.exec(message);
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
export function decodeTile(addr: TileAddr, resp: TileResponse): TileData {
  const nf = resp.extent?.nf ?? resp.grid?.nf, nt = resp.extent?.nt ?? resp.grid?.nt;
  if (!(nf > 0) || !(nt > 0)) throw new TileDecodeError(`tile ${keyOf(addr)}: no grid dimensions`);
  const n = nf * nt;
  const db = resp.grid?.max_db;
  if (!Array.isArray(db) || db.length !== n) {
    throw new TileDecodeError(`tile ${keyOf(addr)}: grid.max_db has ${db?.length ?? "no"} cells, expected ${n}`);
  }
  const cov = coverageCells(addr, resp);
  const frames = resp.grid?.frames;
  const counted = Array.isArray(frames) && frames.length === n ? frames : null;
  const value = new Float32Array(n);
  const state = new Uint8Array(n);
  for (let i = 0; i < n; i++) {
    const s = cov(i);
    if (s === "unobserved") { state[i] = CELL.UNOBSERVED; value[i] = NaN; continue; }
    if (s === "unknown") { state[i] = CELL.UNKNOWN; value[i] = NaN; continue; }
    const v = db[i];
    if (typeof v === "number" && Number.isFinite(v)) { state[i] = CELL.OBSERVED; value[i] = v; continue; }
    value[i] = NaN;
    state[i] = counted && counted[i] === 0 ? CELL.AWAITING : CELL.NO_LEVEL;
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
  };
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
function expandPlane(addr: TileAddr, states: string[], runs: number[], cells: number): string[] {
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
    if (r.status === 503) throw new TileBusyError(capFromRefusal(e.message), e.message);
    throw e;
  }
  return decodeTile(addr, body as TileResponse);
}

/** The lattice bootstrap: one cheap tile (`cells=8`) whose `axes` state the ladder both axes run on. */
export function probeAddr(device = "any", scheme = "view"): TileAddr {
  return { device, scheme, levelF: 0, levelT: 0, fIndex: 0, tIndex: 0, cells: 8 };
}

/** The lattice, read off a probe response. See [[latticeFrom]] for why the client never picks it. */
export function latticeOf(resp: TileResponse, cells: number): Lattice {
  return {
    scheme: String(resp.key.scheme),
    cells,
    f0Hz: resp.axes.frequency.cell_hz / 2 ** resp.key.level_f,
    t0Ns: (resp.axes.time.cell_s * 1e9) / 2 ** resp.key.level_t,
    levelsF: resp.axes.frequency.levels,
    levelsT: resp.axes.time.levels,
  };
}
