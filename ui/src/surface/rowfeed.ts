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
import { expandPlane, isTier, TileDecodeError, weakerTier, type FoldDirection, type StatedTier } from "./tile";
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

/**
 * **What a block's rows were measured at, as the route states it** (T-902, `resolution` on each
 * `rows` message): the honesty tier by the tile route's own rule, the level that answered, and the
 * per-axis fold. A tile built from pushed rows states exactly this — never a tier borrowed from the
 * tile below it — so the pane's level reads what the newest rows actually were.
 */
export interface RowResolution {
  /** `"unknown"` when the block stated no recognised tier: said, never defaulted. */
  readonly tier: StatedTier;
  /** `answered.level`, or -1 when the block did not say. */
  readonly answeredLevel: number;
  readonly fold: { readonly frequency: FoldDirection; readonly time: FoldDirection };
  /** Frequency cells actually measured, `min(fold.frequency.source_cells, nf)`. */
  readonly measuredNf: number;
  /** `fold.time.source_cell / tile_cell` — above 1 on a replicated time axis, else 1. */
  readonly timeStretch: number;
}

const FOLD_ORDER: readonly FoldDirection[] = ["exact", "folded", "replicated"];
const weakerFold = (a: FoldDirection, b: FoldDirection): FoldDirection =>
  FOLD_ORDER.indexOf(a) >= FOLD_ORDER.indexOf(b) ? a : b;

/** Two blocks' claims about one tile, merged to the weaker of each (the tile states the weaker). */
export function mergeResolution(a: RowResolution | null, b: RowResolution): RowResolution {
  if (!a) return b;
  return {
    tier: weakerTier(a.tier, b.tier),
    answeredLevel: Math.max(a.answeredLevel, b.answeredLevel),
    fold: { frequency: weakerFold(a.fold.frequency, b.fold.frequency), time: weakerFold(a.fold.time, b.fold.time) },
    measuredNf: Math.min(a.measuredNf, b.measuredNf),
    timeStretch: Math.max(a.timeStretch, b.timeStretch),
  };
}

/** Reads a block's `resolution` + `answered`. A missing or unrecognised tier is `"unknown"`. */
function parseResolution(j: Record<string, unknown>, nf: number): RowResolution {
  const res = (j.resolution ?? {}) as { source?: unknown; fold?: Record<string, { direction?: unknown; source_cells?: unknown; source_cell?: unknown; tile_cell?: unknown }> };
  const ans = (j.answered ?? {}) as { level?: unknown };
  const dir = (d: unknown): FoldDirection => (FOLD_ORDER.includes(d as FoldDirection) ? (d as FoldDirection) : "exact");
  const ff = res.fold?.frequency, ft = res.fold?.time;
  const srcNf = ff?.source_cells;
  const stretch = typeof ft?.source_cell === "number" && typeof ft?.tile_cell === "number" && ft.tile_cell > 0
    ? Math.max(1, ft.source_cell / ft.tile_cell) : 1;
  return {
    tier: isTier(res.source) ? res.source : "unknown",
    answeredLevel: typeof ans.level === "number" && Number.isInteger(ans.level) ? ans.level : -1,
    fold: { frequency: dir(ff?.direction), time: dir(ft?.direction) },
    measuredNf: typeof srcNf === "number" && Number.isFinite(srcNf) && srcNf > 0 ? Math.min(Math.round(srcNf), nf) : nf,
    timeStretch: Number.isFinite(stretch) ? stretch : 1,
  };
}

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
  /** What these rows were measured at, as the route stated it (T-902). */
  readonly resolution: RowResolution;
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
      return { kind: "rows", row0, rows, nf, maxDb, coverage, resolution: parseResolution(j, nf), final: j.final === true };
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
  /** What the rows filed here were measured at — the weaker of every block's stated claim, or
   * `null` when no measured block has been filed (a grey stretch makes no claim). T-902. */
  resolution: RowResolution | null;
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
      t = { addr, maxDb: new Float32Array(n).fill(NaN), coverage: new Array<string | null>(n).fill(null), rowsSeen: new Uint8Array(col.cells), resolution: null };
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

  /**
   * Forget the pushed rows of `col` in tiles before `tIndex` (T-893). A live feed walks forward for
   * as long as a pane follows, so an accumulator that never forgot would grow with the session; a
   * tile the edge has left is sealed and `GET /api/tiles` is the authority for it from then on.
   */
  prune(col: ColumnAddr, tIndex: number): void {
    const ck = columnKey(col);
    for (const [k, t] of this.tiles) {
      if (t.addr.tIndex < tIndex && columnKey(t.addr) === ck) this.tiles.delete(k);
    }
    const g = this.gaps.get(ck);
    if (g) {
      const keep = g.filter((r) => r[1] > tIndex * col.cells);
      if (keep.length) this.gaps.set(ck, keep); else this.gaps.delete(ck);
    }
  }

  /** Drop one tile's pushed rows (a retune's [[TileCache.invalidate]]). */
  drop(addr: TileAddr): void { this.tiles.delete(keyOf(addr)); }

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
    t.resolution = mergeResolution(t.resolution, m.resolution);
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

/** One column a following pane wants rows pushed for, and the row to start from if it is opened. */
export interface WantedColumn {
  readonly col: ColumnAddr;
  readonly fromRow: number;
}

/**
 * Row subscriptions opened at once by one client (T-893). The route admits
 * [`MAX_ROW_FEEDS`](../../../crates/hk-api/src/rows.rs) = 16 per SERVER, shared by every tab; a
 * full-width pane is six columns, so twelve serves two such panes and leaves room for another client.
 * Past it the columns in excess are simply not subscribed and keep the polling lane (T-460), which
 * is what every column had before.
 */
export const MAX_CLIENT_ROW_FEEDS = 12;
/** How long a column the route refused, or whose socket dropped, waits before it is asked again. */
export const ROW_FEED_RETRY_MS = 5000;

/**
 * **Rows pushed to the columns a FOLLOWING pane draws** (T-893) — the wiring T-468 left undone.
 *
 * The polling lane (`TileCache.refreshEdge`) re-asks a live tile at most once per a share of what
 * it costs, so on a busy route the top of a short following pane was drawn seconds behind the rows
 * that existed. Here each wanted column holds one open-ended subscription (`t_to` absent) that
 * starts at a row the caller names, and every block it pushes goes straight to `sink`, which files
 * it under the tile address the cache already uses. The subscription is still an ADDRESS RANGE
 * ([[rowFeedPath]] refuses anything else); "following" is only that the caller keeps it open.
 *
 * Keyed by COLUMN, not by pane: two panes over one column share one socket, and a column no
 * following pane wants any more is closed on the next [[want]]. A frozen pane never appears in a
 * `want` list — the caller builds it from following viewports only — so pausing closes its feeds,
 * and nothing here can reach a device route: it only ever reads `/ws/tiles/rows`.
 */
export class LiveRowFeeds {
  private feeds = new Map<string, { feed: RowFeed; conn: { close(): void }; col: ColumnAddr }>();
  /** Columns whose last subscription was refused or cut, and when they may be asked again. */
  private retryAt = new Map<string, number>();
  /** Every path this client has opened, in order — what ui/test asserts the request against. */
  readonly requests: string[] = [];

  constructor(
    private readonly open: RowOpener,
    private readonly sink: (col: ColumnAddr, block: RowBlock | GapBlock) => void,
    private readonly opts: { max?: number; retryMs?: number; now?: () => number } = {},
  ) {}

  get size(): number { return this.feeds.size; }
  /** The columns with an open subscription, by column key. */
  columns(): string[] { return [...this.feeds.keys()]; }

  /**
   * Make the open subscriptions exactly `wanted` (first come first served up to the cap): close
   * the rest, open the missing ones. An already-open column is left alone — its own edge carries on.
   */
  want(wanted: readonly WantedColumn[]): void {
    const now = (this.opts.now ?? Date.now)();
    const max = this.opts.max ?? MAX_CLIENT_ROW_FEEDS;
    const keep = new Map<string, WantedColumn>();
    for (const w of wanted) {
      const k = columnKey(w.col);
      if (!keep.has(k) && keep.size < max) keep.set(k, w);
    }
    for (const [k, f] of this.feeds) {
      if (!keep.has(k)) { f.conn.close(); this.feeds.delete(k); }
    }
    for (const [k, w] of keep) {
      if (this.feeds.has(k)) continue;
      if ((this.retryAt.get(k) ?? 0) > now) continue;
      this.subscribe(k, w);
    }
  }

  private subscribe(k: string, w: WantedColumn): void {
    const path = rowFeedPath(w.col, { fromRow: w.fromRow, toRow: null });
    this.requests.push(path);
    const feed = new RowFeed(w.col, { fromRow: w.fromRow, toRow: null });
    const entry = { feed, col: w.col, conn: { close: () => {} } };
    const cut = () => {
      if (this.feeds.get(k) !== entry) return;
      entry.conn.close();
      this.feeds.delete(k);
      this.retryAt.set(k, (this.opts.now ?? Date.now)() + (this.opts.retryMs ?? ROW_FEED_RETRY_MS));
    };
    this.feeds.set(k, entry);
    entry.conn = this.open(path, (text) => {
      if (this.feeds.get(k) !== entry) return;
      let block: RowBlock | GapBlock | null;
      try {
        const m = parseRowMessage(w.col, text);
        block = feed.take(m);
        // A refusal (the route's cap, above all) ends this column's feed; polling carries it.
        if (m.kind === "refused") { cut(); return; }
      } catch {
        // A protocol error is never patched over: drop the feed and start again later from
        // wherever the caller then says the rows in hand stop.
        cut();
        return;
      }
      if (block) this.sink(w.col, block);
    }, cut);
  }

  /** Close every subscription. */
  close(): void {
    for (const f of this.feeds.values()) f.conn.close();
    this.feeds.clear();
  }
}

/**
 * The browser transport for [[LiveRowFeeds]]: one WebSocket per subscription, the token as a query
 * parameter like every `/ws/` route.
 */
export function wsRowOpener(token: string, loc: { protocol: string; host: string } = location): RowOpener {
  return (path, onText, onClose) => {
    const proto = loc.protocol === "https:" ? "wss" : "ws";
    const ws = new WebSocket(`${proto}://${loc.host}${path}&token=${encodeURIComponent(token)}`);
    let closedByUs = false;
    ws.onmessage = (ev) => { if (typeof ev.data === "string") onText(ev.data); };
    ws.onclose = () => { if (!closedByUs) onClose(); };
    ws.onerror = () => { /* onclose follows */ };
    return { close: () => { closedByUs = true; ws.close(); } };
  };
}
