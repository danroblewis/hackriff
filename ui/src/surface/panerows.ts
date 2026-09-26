// **One pane's rows, as this client asks for them and reads them** (T-1043 / LSR-2) — the client half
// of `GET /ws/spectrum/rows` (docs/api.md; the wire is `docs/stream-contract.md` §17).
//
// The route's subscription is a **pane and a time range**: a frequency window, the pane's `nf`
// columns, a row period named on the tile lattice's time axis, and `[t_from, t_to)` in **absolute
// capture time**. The server folds the store's cells onto those columns and pushes one binary block
// per few rows — a 48-byte little-endian header, binary16 values (NaN = *not measured*, never a zero
// and never a floor) and a coverage trailer on the block's own axes.
//
// # What this module is
//
// The **request** and the **decode**, and nothing else: no ring, no texture, no drawing. LSR-1's
// [[LiveRing]] holds the newest rows from the already-open `spectrum/live` socket; this is how a pane
// asks the backend for the rows it does *not* have — the part behind the ring, a scrub into the past,
// and (LSR-3) the gap after a reconnect. Keeping it separate keeps the thin-client rule visible: the
// fold, the quantisation and the coverage are the server's answers, and what happens here is reading
// them exactly as documented, or refusing them.
//
// # The three refusals, and why each is a refusal rather than a default
//
//  1. **No start, no subscription.** [[panePath]] throws without `tFromNs`. There is no way to spell
//     "now" on this route and nothing here invents one: a pane following the live edge and a pane
//     playing back an hour ago are the same request with a different range (the T-420 drift is what
//     two mechanisms cost).
//  2. **An unrecognised code is not read.** A `kind`, a `values` code, a trailer encoding, a tier or
//     a fold direction this build does not know throws [[TileDecodeError]] — the block is not
//     rendered with a guessed meaning. A future encoding gets a new code, never a redefinition.
//  3. **A length that does not add up throws.** `48 + rows × nf × 2 + trailer` is stated by the
//     header, so a short or long message is a corrupt block, not a block to patch.
//
// And one honesty rule carried from the wire: **`DISCONTINUITY` marks every part the store did not
// have** — a grey stretch, or rows a tuned band has no frame for — which is exactly the part a client
// fills from `/api/tiles`. [[PaneBlock.discontinuity]] is that flag; nothing here silently stitches
// across it.
import { f16ToF32, isTier, TileDecodeError, type FoldDirection, type StatedTier } from "./tile";

/** The block header's bytes: fixed by the contract, never inferred from a payload length. */
export const BLOCK_HEADER_BYTES = 48;

/** `kind`: measured rows follow. */
export const KIND_ROWS = 1;
/** `kind`: a stretch the coverage map calls uniformly unobserved — no payload, no trailer. */
export const KIND_UNOBSERVED = 2;

/** `flags` bit 0: no late frame can still land in this block. */
export const FLAG_FINAL = 1;
/** `flags` bit 1: this block does not continue the values delivered so far, or holds rows the store did not have. */
export const FLAG_DISCONTINUITY = 2;

/** `values`: little-endian IEEE binary16. */
export const VALUES_F16_LE = 1;

/** The coverage trailer's only encoding: runs of `(cells, state)` over the block's own cells. */
export const TRAILER_RUN8 = 1;

/** The four coverage states, in the code order the server states in its header. */
export const COVERAGE_STATES = ["unobserved", "observed", "unknown", "excluded"] as const;

/** A coverage state on this wire. */
export type CoverageState = (typeof COVERAGE_STATES)[number];

/** The honesty tiers, by the `tier` byte's code. */
const TIERS: readonly StatedTier[] = ["live-iq", "spectrum-history", "survey-overview"];

/** The fold directions, by the two-bit code each axis carries. */
const FOLDS: readonly FoldDirection[] = ["exact", "folded", "replicated"];

/** The pane a subscription is over: its window, its columns, and whose coverage decides its grey. */
export interface PaneWindow {
  readonly fLoHz: number;
  readonly fHiHz: number;
  /** The pane's columns, 8…4096 — the grid the server folds onto. */
  readonly nf: number;
  /** The lattice time level the row period comes from; `0` is the finest. */
  readonly levelT?: number;
  readonly scheme?: string;
  readonly device?: string;
}

/** The range, in **absolute capture time** (ns). `tToNs: null` runs on into the live edge. */
export interface PaneRange {
  readonly tFromNs: number;
  readonly tToNs: number | null;
}

const isNs = (x: unknown): x is number => typeof x === "number" && Number.isSafeInteger(x) && x >= 0;

/**
 * The request this client builds for a pane subscription (ui/test asserts the request, not only the
 * response — CLAUDE.md's guard against asking for the wrong thing).
 *
 * **Throws without a start**: there is no implicit now, and a client that wants the live edge names
 * the row it already has.
 */
export function panePath(pane: PaneWindow, range: PaneRange, path = "/ws/spectrum/rows"): string {
  if (!Number.isFinite(pane.fLoHz) || !Number.isFinite(pane.fHiHz) || pane.fHiHz <= pane.fLoHz) {
    throw new Error(`a pane needs a window, got [${String(pane.fLoHz)}, ${String(pane.fHiHz)}] Hz`);
  }
  if (!Number.isInteger(pane.nf) || pane.nf < 8 || pane.nf > 4096) {
    throw new Error(`a pane's nf must be an integer in 8..=4096, got ${String(pane.nf)}`);
  }
  if (!isNs(range.tFromNs)) {
    throw new Error(
      `a pane subscription needs a start instant in capture ns, got ${String(range.tFromNs)}`,
    );
  }
  if (range.tToNs !== null && (!isNs(range.tToNs) || range.tToNs <= range.tFromNs)) {
    throw new Error(`tToNs ${String(range.tToNs)} must be an instant after ${range.tFromNs}`);
  }
  const q = new URLSearchParams({
    f_lo_hz: String(pane.fLoHz),
    f_hi_hz: String(pane.fHiHz),
    nf: String(pane.nf),
    t_from: String(range.tFromNs),
  });
  if (range.tToNs !== null) q.set("t_to", String(range.tToNs));
  if (pane.levelT !== undefined && pane.levelT !== 0) q.set("level_t", String(pane.levelT));
  if (pane.scheme !== undefined && pane.scheme !== "view") q.set("scheme", pane.scheme);
  if (pane.device !== undefined && pane.device !== "any") q.set("device", pane.device);
  return `${path}?${q.toString()}`;
}

/** What the first (text) message states back, once checked against what this build can read. */
export interface PaneHeader {
  readonly nf: number;
  readonly fLoHz: number;
  readonly fHiHz: number;
  readonly fCellHz: number;
  readonly tCellS: number;
  readonly levelT: number;
  readonly device: string;
  readonly scheme: string;
  readonly row0: number;
  readonly open: boolean;
  readonly epoch: number;
  readonly rowsPerBlock: number;
  /** The coverage alphabet the blocks' trailer codes index — **served, never assumed**. */
  readonly states: readonly string[];
}

const num = (v: unknown, what: string): number => {
  if (typeof v !== "number" || !Number.isFinite(v)) {
    throw new TileDecodeError(`pane subscription: ${what} is not a number (${JSON.stringify(v)})`);
  }
  return v;
};

/**
 * Reads the `subscribed` header, **refusing a wire this build cannot read** rather than assuming one:
 * the block size, the value encoding, the absent marker, the trailer encoding and the coverage
 * alphabet are all checked here, once per subscription, so a block's codes are never resolved against
 * an alphabet the answer did not state (the tile route's rule, T-467).
 */
export function parsePaneHeader(text: string): PaneHeader {
  const v: unknown = JSON.parse(text);
  if (typeof v !== "object" || v === null) throw new TileDecodeError("pane subscription: not an object");
  const o = v as Record<string, unknown>;
  if (o["type"] !== "subscribed") {
    throw new TileDecodeError(`pane subscription: type ${JSON.stringify(o["type"])}, not "subscribed"`);
  }
  const rec = (o["record"] ?? {}) as Record<string, unknown>;
  if (rec["header_bytes"] !== BLOCK_HEADER_BYTES) {
    throw new TileDecodeError(
      `pane subscription: header_bytes ${String(rec["header_bytes"])}, this client reads ${BLOCK_HEADER_BYTES}`,
    );
  }
  const values = (rec["values"] ?? {}) as Record<string, unknown>;
  if (values["code"] !== VALUES_F16_LE || values["type"] !== "f16" || values["absent"] !== "nan") {
    throw new TileDecodeError(`pane subscription: value encoding ${JSON.stringify(values)} is not f16/nan`);
  }
  const cov = (rec["coverage"] ?? {}) as Record<string, unknown>;
  if (cov["encoding"] !== TRAILER_RUN8) {
    throw new TileDecodeError(`pane subscription: coverage encoding ${String(cov["encoding"])} is not run8`);
  }
  const states = cov["states"];
  if (
    !Array.isArray(states) ||
    states.length !== COVERAGE_STATES.length ||
    states.some((s, i) => s !== COVERAGE_STATES[i])
  ) {
    throw new TileDecodeError(`pane subscription: coverage alphabet ${JSON.stringify(states)} is not the four states`);
  }
  const pane = (o["pane"] ?? {}) as Record<string, unknown>;
  const range = (o["range"] ?? {}) as Record<string, unknown>;
  return {
    nf: num(pane["nf"], "pane.nf"),
    fLoHz: num(pane["f_lo_hz"], "pane.f_lo_hz"),
    fHiHz: num(pane["f_hi_hz"], "pane.f_hi_hz"),
    fCellHz: num(pane["f_cell_hz"], "pane.f_cell_hz"),
    tCellS: num(pane["t_cell_s"], "pane.t_cell_s"),
    levelT: num(pane["level_t"], "pane.level_t"),
    device: String(pane["device"] ?? "any"),
    scheme: String(pane["scheme"] ?? "view"),
    row0: num(range["row0"], "range.row0"),
    open: range["open"] === true,
    epoch: num(o["epoch"], "epoch"),
    rowsPerBlock: num(o["rows_per_block"], "rows_per_block"),
    states: states as string[],
  };
}

/** What every block states about itself, whichever kind it is. */
interface BlockCommon {
  /** The row's address on the level's time axis from the Unix epoch. */
  readonly row0: number;
  readonly rows: number;
  /** The start of `row0`, absolute capture time in ns — exact, from the header's `i64`. */
  readonly t0Ns: number;
  readonly tCellNs: number;
  /** The tuning configurations over the pane's window: a **change** means re-lay the fog. */
  readonly epoch: number;
  /** No late frame can still land in this block. */
  readonly final: boolean;
  /**
   * This block does not continue the values delivered so far, or it holds rows the store had no
   * frame for. **Fill that part from `/api/tiles`** — never stitch across it.
   */
  readonly discontinuity: boolean;
}

/** A block of measurements, folded onto the pane's columns. */
export interface PaneBlock extends BlockCommon {
  readonly kind: "rows";
  readonly nf: number;
  /** The store level that answered. */
  readonly level: number;
  /** The honesty tier **this block** was measured at — never borrowed from a neighbour (T-902). */
  readonly tier: StatedTier;
  readonly fold: { readonly frequency: FoldDirection; readonly time: FoldDirection };
  /** `rows × nf`, row-major; `NaN` is *not measured*, never a level. */
  readonly values: Float32Array;
  /** Cells of `values` that are a measurement, as the server counted them. */
  readonly observedCells: number;
  /** One state code per cell, indexing the header's alphabet. */
  readonly coverage: Uint8Array;
}

/** A stretch the coverage map calls uniformly unobserved: no measurement, and none is coming. */
export interface PaneGapBlock extends BlockCommon {
  readonly kind: "unobserved";
}

/** Either kind of block. */
export type PaneMessage = PaneBlock | PaneGapBlock;

/** Reads an `i64` field as a JS number, refusing one that would lose precision. */
function i64(view: DataView, at: number, what: string): number {
  const v = view.getBigInt64(at, true);
  if (v > BigInt(Number.MAX_SAFE_INTEGER) || v < BigInt(-Number.MAX_SAFE_INTEGER)) {
    throw new TileDecodeError(`pane block: ${what} ${v} is outside the safe integer range`);
  }
  return Number(v);
}

/**
 * Decodes one binary block against the header of the subscription it arrived on.
 *
 * Refuses (rather than guesses) an unknown kind, value encoding, trailer encoding, tier or fold code,
 * a length that does not match `48 + rows × nf × 2 + trailer`, or a trailer whose runs do not cover
 * exactly the block's cells.
 */
export function decodePaneBlock(header: PaneHeader, buf: ArrayBuffer): PaneMessage {
  if (buf.byteLength < BLOCK_HEADER_BYTES) {
    throw new TileDecodeError(`pane block: ${buf.byteLength} B is shorter than its header`);
  }
  const b = new Uint8Array(buf);
  const view = new DataView(buf);
  const kind = b[0], flags = b[1], valueCode = b[2], level = b[3], tierCode = b[4], foldCode = b[5];
  const nf = view.getUint16(6, true);
  const rows = view.getUint32(8, true);
  const epoch = view.getUint32(12, true);
  const t0Ns = i64(view, 16, "t0_ns");
  const tCellNs = i64(view, 24, "t_cell_ns");
  const row0 = i64(view, 32, "row0");
  const trailerBytes = view.getUint32(40, true);
  const observedCells = view.getUint32(44, true);
  if (flags & ~(FLAG_FINAL | FLAG_DISCONTINUITY)) {
    throw new TileDecodeError(`pane block: flags 0b${flags.toString(2)} carry a bit this client does not know`);
  }
  const common: BlockCommon = {
    row0,
    rows,
    t0Ns,
    tCellNs,
    epoch,
    final: (flags & FLAG_FINAL) !== 0,
    discontinuity: (flags & FLAG_DISCONTINUITY) !== 0,
  };
  if (kind === KIND_UNOBSERVED) {
    if (buf.byteLength !== BLOCK_HEADER_BYTES) {
      throw new TileDecodeError(`pane block: an unobserved stretch carries no payload, got ${buf.byteLength} B`);
    }
    return { ...common, kind: "unobserved" };
  }
  if (kind !== KIND_ROWS) {
    throw new TileDecodeError(`pane block: kind ${kind} is not a kind this client reads`);
  }
  if (valueCode !== VALUES_F16_LE) {
    throw new TileDecodeError(`pane block: value encoding ${valueCode} is not f16`);
  }
  if (nf !== header.nf) {
    throw new TileDecodeError(`pane block: ${nf} columns, but the subscription is ${header.nf} wide`);
  }
  const payload = rows * nf * 2;
  if (buf.byteLength !== BLOCK_HEADER_BYTES + payload + trailerBytes) {
    throw new TileDecodeError(
      `pane block: ${buf.byteLength} B is not ${BLOCK_HEADER_BYTES} + ${payload} + ${trailerBytes}`,
    );
  }
  const tier = TIERS[tierCode];
  if (tier === undefined || !isTier(tier)) {
    throw new TileDecodeError(`pane block: tier code ${tierCode} is not a tier this client reads`);
  }
  const fold = { frequency: FOLDS[foldCode & 0b11], time: FOLDS[(foldCode >> 2) & 0b11] };
  if (fold.frequency === undefined || fold.time === undefined || foldCode & 0b1111_0000) {
    throw new TileDecodeError(`pane block: fold code 0b${foldCode.toString(2)} is not a pair of directions`);
  }
  const values = new Float32Array(rows * nf);
  for (let i = 0; i < values.length; i++) {
    values[i] = f16ToF32(view.getUint16(BLOCK_HEADER_BYTES + i * 2, true));
  }
  return {
    ...common,
    kind: "rows",
    nf,
    level,
    tier,
    fold,
    values,
    observedCells,
    coverage: decodeTrailer(header, view, BLOCK_HEADER_BYTES + payload, trailerBytes, rows * nf),
  };
}

/** The coverage trailer: `run8` over the block's own cells, in the payload's order. */
function decodeTrailer(
  header: PaneHeader,
  view: DataView,
  at: number,
  bytes: number,
  cells: number,
): Uint8Array {
  if (bytes < 8) throw new TileDecodeError(`pane block: a ${bytes} B coverage trailer has no header`);
  const encoding = view.getUint8(at);
  if (encoding !== TRAILER_RUN8) {
    throw new TileDecodeError(`pane block: coverage encoding ${encoding} is not run8`);
  }
  const states = view.getUint8(at + 1);
  if (states !== header.states.length) {
    throw new TileDecodeError(`pane block: ${states} coverage states, the subscription stated ${header.states.length}`);
  }
  const runs = view.getUint32(at + 4, true);
  if (bytes !== 8 + runs * 8) {
    throw new TileDecodeError(`pane block: a ${runs}-run trailer is ${8 + runs * 8} B, got ${bytes}`);
  }
  const out = new Uint8Array(cells);
  let cell = 0;
  for (let r = 0; r < runs; r++) {
    const n = view.getUint32(at + 8 + r * 8, true);
    const state = view.getUint8(at + 8 + r * 8 + 4);
    if (state >= header.states.length) {
      throw new TileDecodeError(`pane block: coverage code ${state} is not in the served alphabet`);
    }
    if (cell + n > cells) {
      throw new TileDecodeError(`pane block: coverage runs overrun ${cells} cells`);
    }
    out.fill(state, cell, cell + n);
    cell += n;
  }
  if (cell !== cells) {
    throw new TileDecodeError(`pane block: coverage runs cover ${cell} cells, expected ${cells}`);
  }
  return out;
}

/** `true` where this block's cell is a measurement of its own extent (the ramp's only input). */
export const isObserved = (block: PaneBlock, cell: number): boolean =>
  block.coverage[cell] === COVERAGE_STATES.indexOf("observed");
