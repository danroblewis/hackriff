// T-468: the client half of `GET /ws/tiles/rows` — rows pushed to a subscription over an ADDRESS
// RANGE, filed by tile address, with one edge per pane rather than one per client.
//
// These assert the REQUEST the client builds (CLAUDE.md: contract tests cover the server; the guard
// against a client asking for the wrong thing lives here), and they fail if a subscription could be
// spelled as "now".
import { test } from "node:test";
import assert from "node:assert/strict";
import { keyOf } from "../src/surface/lattice";
import {
  PaneFeeds, parseRowMessage, rowAt, rowFeedPath, type ColumnAddr, type RowOpener,
} from "../src/surface/rowfeed";

const CELLS = 8;
const col: ColumnAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 3, cells: CELLS };
const T_CELL_S = 0.04;

const subscribed = (from: number, to: number | null) => JSON.stringify({
  type: "subscribed", range: { t_from: from, t_to: to, open: to === null }, extent: { t_cell_s: T_CELL_S, nf: CELLS },
});
/** A block whose row k has column (k mod CELLS) hot — the address is in the data. */
const rows = (row0: number, n: number) => {
  const maxDb: (number | null)[] = [];
  for (let r = row0; r < row0 + n; r++) for (let f = 0; f < CELLS; f++) maxDb.push(f === r % CELLS ? -40 : -100);
  return JSON.stringify({
    type: "rows", row0, rows: n, nf: CELLS, max_db: maxDb, final: false,
    tile: { t_index: Math.floor(row0 / CELLS), row: row0 % CELLS },
    coverage: { encoding: "plane-rle", states: ["unobserved", "observed", "unknown", "excluded"], nt: n, nf: CELLS, aligned: true, plane: { runs: [1, n * CELLS] } },
  });
};
const end = (row: number) => JSON.stringify({ type: "end", row, reason: "range-complete" });

/** A fake transport: records every path asked for and lets the test push messages per path. */
function fakeOpener() {
  const opened: string[] = [];
  const push = new Map<string, (t: string) => void>();
  const closed: string[] = [];
  const open: RowOpener = (path, onText) => {
    opened.push(path);
    push.set(path, onText);
    return { close: () => closed.push(path) };
  };
  return { open, opened, closed, send: (path: string, ...msgs: string[]) => msgs.forEach((m) => push.get(path)!(m)) };
}

test("the request is an address range, and there is no way to spell 'now'", () => {
  const p = rowFeedPath(col, { fromRow: 1200, toRow: null });
  const q = new URLSearchParams(p.split("?")[1]);
  assert.ok(p.startsWith("/ws/tiles/rows?"));
  assert.equal(q.get("t_from"), "1200");
  assert.equal(q.get("t_to"), null, "an open range sends no end");
  assert.equal(q.get("t_index"), null, "a range is not one tile");
  assert.equal(q.get("f_index"), "3");
  assert.equal(q.get("cells"), "8");

  const closed = new URLSearchParams(rowFeedPath(col, { fromRow: 10, toRow: 20 }).split("?")[1]);
  assert.equal(closed.get("t_from"), "10");
  assert.equal(closed.get("t_to"), "20");

  // No start row, no request: nothing defaults one.
  for (const fromRow of [undefined, null, NaN, -1, 1.5] as unknown as number[]) {
    assert.throws(() => rowFeedPath(col, { fromRow, toRow: null }), /start row/);
  }
  assert.throws(() => rowFeedPath(col, { fromRow: 5, toRow: 5 }));
});

test("rowAt maps a capture instant to its row on the level's own axis", () => {
  assert.equal(rowAt(0, 40e6), 0);
  assert.equal(rowAt(79_999_999, 40e6), 1);
  assert.equal(rowAt(80e6, 40e6), 2);
});

test("one edge per PANE: a live pane and a playback pane advance on their own rows", () => {
  const t = fakeOpener();
  const touched: string[] = [];
  const feeds = new PaneFeeds(t.open, (pane, tiles) => touched.push(`${pane}:${tiles.map((a) => a.tIndex).join(",")}`));
  const live = feeds.subscribe("a", col, { fromRow: 100, toRow: null });
  const past = feeds.subscribe("b", col, { fromRow: 10, toRow: 20 });
  assert.deepEqual(t.opened, [live, past], "two subscriptions, two requests, from one client");

  t.send(live, subscribed(100, null), rows(100, 4));
  t.send(past, subscribed(10, 20), rows(10, 6));
  assert.equal(feeds.feed("a")!.edgeRow, 104);
  assert.equal(feeds.feed("b")!.edgeRow, 16);
  assert.notEqual(feeds.edgeNs("a"), feeds.edgeNs("b"), "two panes, two edges");
  assert.equal(feeds.edgeNs("a"), 104 * T_CELL_S * 1e9);

  // The playback range finishes; the live one keeps going, untouched by it.
  t.send(past, rows(16, 4), end(20));
  assert.equal(feeds.feed("b")!.done, true);
  assert.equal(feeds.feed("b")!.edgeRow, 20);
  t.send(live, rows(104, 1));
  assert.equal(feeds.feed("a")!.edgeRow, 105);
  assert.equal(feeds.feed("a")!.done, false);
  assert.deepEqual(touched, ["a:12", "b:1", "b:2", "a:13"], "each block patches the tile(s) its rows belong to");
});

test("rows are filed by TILE ADDRESS, each at its own row", () => {
  const t = fakeOpener();
  const feeds = new PaneFeeds(t.open);
  const p = feeds.subscribe("a", col, { fromRow: 6, toRow: 12 });
  t.send(p, subscribed(6, 12), rows(6, 2), rows(8, 4));
  const t0 = feeds.rows.get({ ...col, tIndex: 0 })!, t1 = feeds.rows.get({ ...col, tIndex: 1 })!;
  assert.deepEqual([...t0.rowsSeen], [0, 0, 0, 0, 0, 0, 1, 1]);
  assert.deepEqual([...t1.rowsSeen], [1, 1, 1, 1, 0, 0, 0, 0]);
  for (const [tile, r, abs] of [[t0, 6, 6], [t0, 7, 7], [t1, 0, 8], [t1, 3, 11]] as const) {
    const row = tile.maxDb.slice(r * CELLS, (r + 1) * CELLS);
    assert.equal(row.indexOf(-40), abs % CELLS, `row ${abs} at its own address`);
    assert.equal(tile.coverage[r * CELLS], "observed");
  }
  assert.ok(Number.isNaN(t0.maxDb[0]), "a row not pushed is not measured, never zero");
  assert.equal(t0.coverage[0], null, "and not a coverage claim either");
  assert.equal(keyOf(t1.addr), keyOf({ ...col, tIndex: 1 }));
});

test("an unobserved stretch is filed as a ROW RANGE, never as tiles", () => {
  const t = fakeOpener();
  const feeds = new PaneFeeds(t.open);
  const p = feeds.subscribe("a", col, { fromRow: 4, toRow: 30 });
  t.send(p, subscribed(4, 30), JSON.stringify({ type: "unobserved", row0: 4, rows: 26, final: true }), end(30));
  assert.equal(feeds.rows.size, 0, "grey allocates no tile");
  assert.deepEqual(feeds.rows.gapsOf(col), [[4, 30]]);
  assert.equal(feeds.rows.greyAt(col, 3), false);
  assert.equal(feeds.rows.greyAt(col, 4), true);
  assert.equal(feeds.rows.greyAt(col, 29), true);
  assert.equal(feeds.rows.greyAt(col, 30), false);
  assert.equal(feeds.feed("a")!.done, true);
});

test("a gap of billions of rows costs two numbers, and adjacent gaps merge", () => {
  const t = fakeOpener();
  const feeds = new PaneFeeds(t.open);
  const p = feeds.subscribe("a", col, { fromRow: 0, toRow: null });
  const huge = 4096 * 2 ** 30;
  t.send(p, subscribed(0, null),
    JSON.stringify({ type: "unobserved", row0: 0, rows: 4096, final: true }),
    JSON.stringify({ type: "unobserved", row0: 4096, rows: huge, final: true }),
    rows(4096 + huge, 2));
  assert.deepEqual(feeds.rows.gapsOf(col), [[0, 4096 + huge]]);
  assert.equal(feeds.rows.size, 1, "only the tile the real rows landed in");
  assert.equal(feeds.rows.greyAt(col, 4096 + huge), false);
  assert.equal(feeds.feed("a")!.edgeRow, 4096 + huge + 2);
});

test("a skipped or repeated row is a protocol error, never patched over", () => {
  const t = fakeOpener();
  const feeds = new PaneFeeds(t.open);
  const p = feeds.subscribe("a", col, { fromRow: 0, toRow: null });
  t.send(p, subscribed(0, null), rows(0, 3));
  assert.throws(() => t.send(p, rows(4, 1)), /expected 3/);
  assert.throws(() => t.send(p, rows(2, 1)), /expected 3/);
});

test("a block that does not add up fails closed", () => {
  const good = JSON.parse(rows(0, 2));
  const variants: Record<string, unknown>[] = [
    { ...good, max_db: good.max_db.slice(1) },
    { ...good, coverage: { ...good.coverage, plane: { runs: [1, 15] } } },
    { ...good, coverage: { ...good.coverage, plane: { runs: [9, 16] } } },
    { ...good, coverage: { ...good.coverage, aligned: false } },
    { ...good, row0: 7 },
    { ...good, nf: 16 },
  ];
  for (const v of variants) assert.throws(() => parseRowMessage(col, JSON.stringify(v)));
  assert.equal(parseRowMessage(col, rows(0, 2)).kind, "rows");
});

test("re-subscribing a pane closes its old feed and starts a new edge", () => {
  const t = fakeOpener();
  const feeds = new PaneFeeds(t.open);
  const first = feeds.subscribe("a", col, { fromRow: 0, toRow: null });
  const second = feeds.subscribe("a", col, { fromRow: 500, toRow: 600 });
  assert.deepEqual(t.closed, [first]);
  assert.equal(feeds.feed("a")!.edgeRow, 500);
  assert.notEqual(first, second);
});
