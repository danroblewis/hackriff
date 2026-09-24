// **Pushed rows, per pane, keyed by tile address** (T-468) — the client half of
// `GET /ws/tiles/rows` (docs/api.md).
//
// The route pushes rows as they are recorded to a subscription over an **address range** of the tile
// lattice: one tile column `(scheme, device, level_f, f_index, level_t, cells)` and a row range
// `[t_from, t_to)` on that level's time axis. Row `r` is row `r mod cells` of tile
// `t_index = r div cells`, so everything that arrives is filed under the tile address the cache
// already uses — the accumulator here is keyed by [[keyOf]], never by "the live stream".
//
// **Why a range and not "live".** Historical playback (T-463) is a reader walking forward through
// sealed history, a second position beside the growing edge. A subscription that could only mean
// "now" could not serve it, and playback would grow its own mechanism — the drift that T-420, T-388,
// T-397 and T-412 each were. So [[rowFeedPath]] refuses to build a request without a start row, and
// nothing here defaults one.
//
// **One edge per pane, not one per client.** `SurfacePreview.edgeNs` is one monotone scalar and
// `atEdge()` takes one edge; that is the single-edge assumption this module does not share. Each
// subscription carries its **own** edge — the next row it expects — so a pane following the live
// edge and a pane playing back an hour ago each advance on their own rows ([[PaneFeeds.edgeNs]]).
//
// Presentation only: the server decides what a row holds and whether it is grey (each block's
// `coverage`); this files the numbers where they belong and refuses anything that does not add up.
import { expandPlane, TileDecodeError } from "./tile";
import { keyOf, type TileAddr } from "./lattice";

/** A tile column: a tile address without its time index. */
export type ColumnAddr = Omit<TileAddr, "tIndex">;

/** A row range on the column's time axis. `toRow: null` runs on until the subscription is closed. */
export interface RowRange {
  readonly fromRow: number;
  readonly toRow: number | null;
}

const isRow = (x: unknown): x is number => typeof x === "number" && Number.isSafeInteger(x) && x >= 0;

/**
 * The request this client builds for a subscription (ui/test asserts the request, not only the
 * response). **Throws without a start row**: there is no implicit now.
 */
export function rowFeedPath(col: ColumnAddr, range: RowRange, path = "/ws/tiles/rows"): string {
  if (!isRow(range.fromRow)) throw new Error(`a row subscription needs a start row, got ${String(range.fromRow)}`);
  if (range.toRow !== null && (!isRow(range.toRow) || range.toRow <= range.fromRow)) {
    throw new Error(`toRow ${String(range.toRow)} must be a row after ${range.fromRow}`);
  }
  const q = new URLSearchParams({
    level_f: String(col.levelF),
    level_t: String(col.levelT),
    f_index: String(col.fIndex),
    t_from: String(range.fromRow),
  });
  if (range.toRow !== null) q.set("t_to", String(range.toRow));
  if (col.scheme !== "view") q.set("scheme", col.scheme);
  if (col.device !== "any") q.set("device", col.device);
  if (col.cells !== 256) q.set("cells", String(col.cells));
  return `${path}?${q.toString()}`;
}

/** The row address holding capture instant `tNs` at a level whose time cell is `tCellNs`. */
export const rowAt = (tNs: number, tCellNs: number): number => Math.floor(tNs / tCellNs);

/** One decoded block of rows, filed under the tile it patches. */
export interface RowBlock {
  readonly kind: "rows";
  readonly row0: number;
  readonly rows: number;
  readonly nf: number;
  /** `rows × nf`, row-major; `NaN` is not measured (never quiet). */
  readonly maxDb: Float32Array;
  /** One coverage state per cell, from the block's own plane. */
  readonly coverage: readonly string[];
  readonly final: boolean;
}

/** A stretch the coverage map calls unobserved, possibly over several tiles. */
export interface GapBlock { readonly kind: "unobserved"; readonly row0: number; readonly rows: number; readonly final: boolean }

export type RowMessage =
  | { readonly kind: "subscribed"; readonly fromRow: number; readonly toRow: number | null; readonly tCellS: number }
  | RowBlock
  | GapBlock
  | { readonly kind: "end"; readonly row: number }
  | { readonly kind: "refused"; readonly status: number; readonly reason: string };

/** Parses one text message from the route, failing closed on anything that does not add up. */
export function parseRowMessage(col: ColumnAddr, text: string): RowMessage {
  const j = JSON.parse(text) as Record<string, unknown>;
  const bad = (why: string) => new TileDecodeError(`row feed ${keyOf({ ...col, tIndex: -1 })}: ${why}`);
  switch (j.type) {
    case "subscribed": {
      const r = j.range as Record<string, unknown> | undefined;
      const e = j.extent as Record<string, unknown> | undefined;
      if (!r || !isRow(r.t_from) || !(r.t_to === null || isRow(r.t_to))) throw bad("subscribed without a range");
      if (!e || typeof e.t_cell_s !== "number" || !(e.t_cell_s > 0)) throw bad("subscribed without a time cell");
      return { kind: "subscribed", fromRow: r.t_from, toRow: r.t_to as number | null, tCellS: e.t_cell_s };
    }
    case "rows": {
      const { row0, rows, nf } = j;
      if (!isRow(row0) || !isRow(rows) || rows === 0 || nf !== col.cells) throw bad("rows without a shape");
      if ((row0 % col.cells) + rows > col.cells) throw bad(`block [${row0}, +${rows}) crosses a tile`);
      const db = j.max_db;
      if (!Array.isArray(db) || db.length !== rows * nf) throw bad(`max_db is not ${rows} x ${nf}`);
      const maxDb = Float32Array.from(db, (x) => (x === null ? NaN : Number(x)));
      const cov = j.coverage as { states?: string[]; nt?: number; nf?: number; aligned?: boolean; plane?: { runs: number[] } } | undefined;
      if (!cov || !Array.isArray(cov.states) || cov.aligned !== true || cov.nt !== rows || cov.nf !== nf || !cov.plane) {
        throw bad("a block's coverage is not on its own axes");
      }
      const coverage = expandPlane({ ...col, tIndex: Math.floor(row0 / col.cells) }, cov.states, cov.plane.runs, rows * nf);
      return { kind: "rows", row0, rows, nf, maxDb, coverage, final: j.final === true };
    }
    case "unobserved":
      if (!isRow(j.row0) || !isRow(j.rows) || j.rows === 0) throw bad("unobserved without a stretch");
      return { kind: "unobserved", row0: j.row0, rows: j.rows, final: j.final === true };
    case "end":
      if (!isRow(j.row)) throw bad("end without a row");
      return { kind: "end", row: j.row };
    case "refused":
      return { kind: "refused", status: Number(j.status), reason: String(j.reason ?? "") };
    default:
      throw bad(`unknown message type ${String(j.type)}`);
  }
}

/** One tile's pushed rows. `rowsSeen[r]` says whether row `r` of the tile has arrived. */
export interface TileRows {
  readonly addr: TileAddr;
  readonly maxDb: Float32Array;
  readonly coverage: (string | null)[];
  readonly rowsSeen: Uint8Array;
}

/** A column key: a tile key without its time index. */
const columnKey = (col: ColumnAddr): string => keyOf({ ...col, tIndex: -1 });

/**
 * Rows filed by **tile address**. Two panes looking at the same place share one entry — the
 * accumulator is a property of the lattice, not of whoever subscribed.
 *
 * **Grey stretches are kept as ROW RANGES, never as tiles.** An `unobserved` message may span
 * billions of rows (the route doubles its probe across grey, and `t_from=0` is a normal request), so
 * expanding one into filled tiles would allocate a 256 × 256 plane per tile it crosses — an hour of
 * an un-tuned band at the finest level is ~350 tiles. A range costs two numbers however long it is;
 * a renderer asks [[greyAt]] / [[gapsOf]] for the rows it is drawing.
 */
export class RowAccumulator {
  private tiles = new Map<string, TileRows>();
  /** Per column: disjoint, sorted, merged `[row0, row1)` stretches the coverage map calls unobserved. */
  private gaps = new Map<string, [number, number][]>();

  get(addr: TileAddr): TileRows | undefined { return this.tiles.get(keyOf(addr)); }
  /** Tiles holding pushed rows. Grey stretches allocate none. */
  get size(): number { return this.tiles.size; }

  /** The unobserved stretches filed for `col`, sorted and merged. */
  gapsOf(col: ColumnAddr): readonly (readonly [number, number])[] { return this.gaps.get(columnKey(col)) ?? []; }

  /** Whether row `row` of `col` lies in a filed unobserved stretch. */
  greyAt(col: ColumnAddr, row: number): boolean {
    const g = this.gaps.get(columnKey(col));
    if (!g) return false;
    let lo = 0, hi = g.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (g[mid][1] <= row) lo = mid + 1; else hi = mid;
    }
    return lo < g.length && g[lo][0] <= row;
  }

  private tile(col: ColumnAddr, tIndex: number): TileRows {
    const addr: TileAddr = { ...col, tIndex };
    const k = keyOf(addr);
    let t = this.tiles.get(k);
    if (!t) {
      const n = col.cells * col.cells;
      t = { addr, maxDb: new Float32Array(n).fill(NaN), coverage: new Array<string | null>(n).fill(null), rowsSeen: new Uint8Array(col.cells) };
      this.tiles.set(k, t);
    }
    return t;
  }

  private addGap(col: ColumnAddr, a: number, b: number): void {
    const k = columnKey(col);
    const g = this.gaps.get(k) ?? [];
    g.push([a, b]);
    g.sort((x, y) => x[0] - y[0]);
    const merged: [number, number][] = [];
    for (const r of g) {
      const last = merged[merged.length - 1];
      if (last && r[0] <= last[1]) last[1] = Math.max(last[1], r[1]); else merged.push([r[0], r[1]]);
    }
    this.gaps.set(k, merged);
  }

  /** Files a block; returns the tile addresses whose pushed rows it changed (none for a gap). */
  apply(col: ColumnAddr, m: RowBlock | GapBlock): TileAddr[] {
    if (m.kind === "unobserved") {
      this.addGap(col, m.row0, m.row0 + m.rows);
      return [];
    }
    const c = col.cells;
    const tIndex = Math.floor(m.row0 / c);
    const t = this.tile(col, tIndex);
    for (let src = 0; src < m.rows; src++) {
      const y = m.row0 + src - tIndex * c;
      for (let f = 0; f < c; f++) {
        t.maxDb[y * c + f] = m.maxDb[src * c + f];
        t.coverage[y * c + f] = m.coverage[src * c + f];
      }
      t.rowsSeen[y] = 1;
    }
    return [t.addr];
  }
}

/**
 * One subscription and **its own edge**: the next row it expects. Rows arrive in order, once each —
 * a block that does not start at the edge is a protocol error, never patched over.
 */
export class RowFeed {
  private edge: number;
  private ended = false;
  tCellNs: number | null = null;

  constructor(readonly col: ColumnAddr, readonly range: RowRange) { this.edge = range.fromRow; }

  /** The next row this feed will receive. */
  get edgeRow(): number { return this.edge; }
  get done(): boolean { return this.ended; }
  /** This feed's edge on the capture clock, or `null` before the route stated its time cell. */
  get edgeNs(): number | null { return this.tCellNs === null ? null : this.edge * this.tCellNs; }

  /** Takes one message; returns a block to file, or `null`. */
  take(m: RowMessage): RowBlock | GapBlock | null {
    switch (m.kind) {
      case "subscribed":
        if (m.fromRow !== this.range.fromRow || m.toRow !== this.range.toRow) {
          throw new TileDecodeError(`subscribed to [${m.fromRow}, ${String(m.toRow)}), asked for [${this.range.fromRow}, ${String(this.range.toRow)})`);
        }
        this.tCellNs = m.tCellS * 1e9;
        return null;
      case "rows":
      case "unobserved":
        if (m.row0 !== this.edge) throw new TileDecodeError(`rows from ${m.row0}, expected ${this.edge}`);
        if (this.range.toRow !== null && m.row0 + m.rows > this.range.toRow) throw new TileDecodeError("rows past the range");
        this.edge = m.row0 + m.rows;
        return m;
      case "end":
        if (m.row !== this.edge) throw new TileDecodeError(`ended at ${m.row} with rows up to ${this.edge}`);
        this.ended = true;
        return null;
      case "refused":
        this.ended = true;
        return null;
    }
  }
}

/** The transport: open `path`, call `onText` per message; returns a closer. Injected for tests. */
export type RowOpener = (path: string, onText: (text: string) => void, onClose: () => void) => { close(): void };

/**
 * Row subscriptions **per pane**, filing into one shared accumulator. Each pane's edge is its own —
 * the property `SurfacePreview.edgeNs` (one scalar per client) does not have.
 */
export class PaneFeeds {
  readonly rows = new RowAccumulator();
  private feeds = new Map<string, { feed: RowFeed; conn: { close(): void } }>();

  constructor(private readonly open: RowOpener, private readonly onRows: (pane: string, tiles: TileAddr[]) => void = () => {}) {}

  /** (Re)subscribes `pane` to `range` of `col`; returns the path it asked for. */
  subscribe(pane: string, col: ColumnAddr, range: RowRange): string {
    this.close(pane);
    const path = rowFeedPath(col, range);
    const feed = new RowFeed(col, range);
    const conn = this.open(path, (text) => {
      const block = feed.take(parseRowMessage(col, text));
      if (block) this.onRows(pane, this.rows.apply(col, block));
    }, () => { /* the feed's own `done` says whether it ended or was cut */ });
    this.feeds.set(pane, { feed, conn });
    return path;
  }

  close(pane: string): void {
    this.feeds.get(pane)?.conn.close();
    this.feeds.delete(pane);
  }

  feed(pane: string): RowFeed | undefined { return this.feeds.get(pane)?.feed; }
  /** This pane's own edge, ns, or `null` when it has none yet. */
  edgeNs(pane: string): number | null { return this.feeds.get(pane)?.feed.edgeNs ?? null; }
}
