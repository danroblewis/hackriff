// T-080: candidates/confirmed inventory split, promote/delete (auth + refresh), the removed
// first/last-seen columns, and the sidebar layout. No DOM is available under node:test, so the
// DOM-independent query/action functions are tested directly, and the layout checks read
// index.html as text (same technique as reading a fixture file).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ControlClient, type FetchFn } from "../src/controls/client";
import {
  type Filters, type InventoryClient, type Row, type SortState,
  deleteEntry, inventoryQuery, loadInventoryPage, nextSort, promoteEntry, sortRows,
} from "../src/inventory";

// esbuild bundles this file to ui/node_modules/hk-ui-test/, so a URL relative to import.meta.url
// would resolve under node_modules; `npm test` always runs with cwd = ui/ (justfile's `test-ui`
// does `cd ui` first), so a cwd-relative path is the reliable one.
const html = readFileSync("src/index.html", "utf8");

function mkRow(over: Partial<Row> & { id: string; state: Row["state"] }): Row {
  return {
    f_center_hz: 101.3e6, bandwidth_hz: 150e3, f_lo_hz: 101.225e6, f_hi_hz: 101.375e6,
    first_seen_s: 1_789_300_800, last_seen_s: 1_789_300_920, count: 1,
    known_status: "known", status: null, tags: [], family: null,
    identity_scheme: null, identity_class: null, withheld: false, recurrence: null,
    ...over,
  };
}

// ---- query building + split-by-state loading ----

test("inventoryQuery carries state, the shared filters, limit and cursor", () => {
  const f: Filters = { fLoHz: 1e6, fHiHz: 2e6, t0: 10, t1: 20, status: "known", tag: "x" };
  const p = new URLSearchParams(inventoryQuery("candidate", f, "cur1").split("?")[1]);
  assert.equal(p.get("state"), "candidate");
  assert.equal(p.get("f_lo"), "1000000");
  assert.equal(p.get("f_hi"), "2000000");
  assert.equal(p.get("t0"), "10");
  assert.equal(p.get("t1"), "20");
  assert.equal(p.get("status"), "known");
  assert.equal(p.get("tag"), "x");
  assert.equal(p.get("cursor"), "cur1");
  assert.equal(p.get("limit"), "200");
});

test("candidates and confirmed load from their own state query and get back only their own rows", async () => {
  const calls: string[] = [];
  const candRow = mkRow({ id: "c1", state: "candidate" });
  const confRow = mkRow({ id: "f1", state: "confirmed" });
  const client: InventoryClient = {
    get: async (path: string) => {
      calls.push(path);
      if (path.includes("state=candidate")) return { entries: [candRow], next_cursor: null } as never;
      if (path.includes("state=confirmed")) return { entries: [confRow], next_cursor: null } as never;
      throw new Error(`unexpected path ${path}`);
    },
    post: async () => { throw new Error("not used"); },
    del: async () => { throw new Error("not used"); },
  };
  const cands = await loadInventoryPage(client, "candidate", {});
  const confs = await loadInventoryPage(client, "confirmed", {});
  assert.deepEqual(cands.entries.map((r) => r.id), ["c1"]);
  assert.deepEqual(confs.entries.map((r) => r.id), ["f1"]);
  assert.ok(calls[0].includes("state=candidate"), calls[0]);
  assert.ok(calls[1].includes("state=confirmed"), calls[1]);
});

// ---- promote / delete: auth header + refresh ----

function fakeFetch(handler: (url: string, init: RequestInit) => { status: number; body: unknown }): { fn: FetchFn; seen: { url: string; init: RequestInit }[] } {
  const seen: { url: string; init: RequestInit }[] = [];
  const fn: FetchFn = async (url, init) => {
    seen.push({ url, init });
    const { status, body } = handler(url, init);
    return { ok: status >= 200 && status < 300, status, statusText: "", json: async () => body };
  };
  return { fn, seen };
}

test("promote sends an authenticated POST and reloads the lists on success", async () => {
  const { fn, seen } = fakeFetch(() => ({ status: 200, body: { changed: true, entry: {} } }));
  const client = new ControlClient("tok-123", fn);
  let reloaded = 0;
  const res = await promoteEntry(client, "e1", async () => { reloaded++; });
  assert.equal(res.ok, true);
  assert.equal(reloaded, 1);
  assert.equal(seen.length, 1);
  assert.equal(seen[0].url, "/api/inventory/e1/promote");
  assert.equal(seen[0].init.method, "POST");
  assert.equal((seen[0].init.headers as Record<string, string>).Authorization, "Bearer tok-123");
});

test("delete sends an authenticated DELETE (id encoded) and reloads the lists on success", async () => {
  const { fn, seen } = fakeFetch(() => ({ status: 200, body: { deleted: {} } }));
  const client = new ControlClient("tok-456", fn);
  let reloaded = 0;
  const res = await deleteEntry(client, "e2 with space", async () => { reloaded++; });
  assert.equal(res.ok, true);
  assert.equal(reloaded, 1);
  assert.equal(seen[0].url, "/api/inventory/e2%20with%20space");
  assert.equal(seen[0].init.method, "DELETE");
  assert.equal((seen[0].init.headers as Record<string, string>).Authorization, "Bearer tok-456");
});

test("a refused promote surfaces the server's {error, code} and does not reload", async () => {
  const { fn } = fakeFetch(() => ({ status: 404, body: { error: "unknown id", code: "not_found" } }));
  const client = new ControlClient("tok", fn);
  let reloaded = 0;
  const res = await promoteEntry(client, "missing", async () => { reloaded++; });
  assert.equal(res.ok, false);
  assert.ok(!res.ok && res.message.includes("unknown id"), JSON.stringify(res));
  assert.ok(!res.ok && res.message.includes("not_found"), JSON.stringify(res));
  assert.equal(reloaded, 0, "a refused action changed nothing server-side, so the list is not reloaded");
});

test("a refused delete surfaces the server's {error, code} and does not reload", async () => {
  const { fn } = fakeFetch(() => ({ status: 409, body: { error: "already deleted", code: "conflict" } }));
  const client = new ControlClient("tok", fn);
  let reloaded = 0;
  const res = await deleteEntry(client, "e3", async () => { reloaded++; });
  assert.equal(res.ok, false);
  assert.ok(!res.ok && res.message.includes("already deleted") && res.message.includes("conflict"), JSON.stringify(res));
  assert.equal(reloaded, 0);
});

// ---- sorting (T-083): default frequency ascending, header-click toggle, missing values last ----

test("default order is frequency ascending", () => {
  const rows = [
    mkRow({ id: "a", state: "candidate", f_center_hz: 200e6 }),
    mkRow({ id: "b", state: "candidate", f_center_hz: 50e6 }),
    mkRow({ id: "c", state: "candidate", f_center_hz: 101e6 }),
  ];
  const sorted = sortRows(rows, "freq", 1);
  assert.deepEqual(sorted.map((r) => r.id), ["b", "c", "a"]);
});

test("clicking a header sorts by it; clicking the same header again toggles direction", () => {
  let sort: SortState = { key: "freq", dir: 1 };
  sort = nextSort(sort, "family"); // a different column: starts ascending
  assert.deepEqual(sort, { key: "family", dir: 1 });
  sort = nextSort(sort, "family"); // same column again: toggles
  assert.deepEqual(sort, { key: "family", dir: -1 });
  sort = nextSort(sort, "family");
  assert.deepEqual(sort, { key: "family", dir: 1 });
});

test("count and recurrence default to busiest-first on their first click", () => {
  const fresh: SortState = { key: "freq", dir: 1 };
  assert.deepEqual(nextSort(fresh, "count"), { key: "count", dir: -1 });
  assert.deepEqual(nextSort(fresh, "recurrence"), { key: "recurrence", dir: -1 });
});

test("recurrence sorts by occurrences; rows with no recurrence data sort last in either direction", () => {
  const rows = [
    mkRow({ id: "none", state: "candidate", recurrence: null }),
    mkRow({ id: "few", state: "candidate", recurrence: { occurrences: 2, appearances: 1, span_s: 10, on_air_s: 1, duty_cycle: 0.1 } }),
    mkRow({ id: "many", state: "candidate", recurrence: { occurrences: 40, appearances: 5, span_s: 100, on_air_s: 80, duty_cycle: 0.8 } }),
  ];
  assert.deepEqual(sortRows(rows, "recurrence", -1).map((r) => r.id), ["many", "few", "none"]);
  assert.deepEqual(sortRows(rows, "recurrence", 1).map((r) => r.id), ["few", "many", "none"]);
});

test("sort state (not reset by loading new rows) still orders a fresh page the same way", () => {
  // Simulates a header click, then a reload (Promote/Delete) bringing in a different rows array;
  // the sort key/direction is a list's own field, untouched by loading, so re-applying it to the
  // new rows reproduces the chosen order.
  let sort: SortState = { key: "freq", dir: 1 };
  sort = nextSort(sort, "count");
  const reloaded = [
    mkRow({ id: "x", state: "candidate", count: 5 }),
    mkRow({ id: "y", state: "candidate", count: 20 }),
    mkRow({ id: "z", state: "candidate", count: 1 }),
  ];
  assert.deepEqual(sortRows(reloaded, sort.key, sort.dir).map((r) => r.id), ["y", "x", "z"]);
});

test("index.html: candidate/confirmed recurrence columns and the selections frequency column are sortable", () => {
  assert.ok((html.match(/data-key="recurrence"/g) ?? []).length === 2, "both inventory tables sort by recurrence");
  const selStart = html.indexOf('<table id="sel-table"');
  const selEnd = html.indexOf("</table>", selStart);
  assert.ok(/data-key="freq"/.test(html.slice(selStart, selEnd)), "selections table sorts by frequency");
});

// ---- layout: first/last-seen columns gone, sidebar present ----

test("the inventory tables no longer have first/last-seen columns", () => {
  assert.ok(!html.includes('data-key="first"'), "no first-seen sort column");
  assert.ok(!html.includes('data-key="last"'), "no last-seen sort column");
  assert.ok(!/>\s*first seen\s*</.test(html), "no 'first seen' header text");
  assert.ok(!/>\s*last seen\s*</.test(html), "no 'last seen' header text");
});

test("the inventory tables keep the other columns (frequency, family, identity, count)", () => {
  assert.ok(html.includes('data-key="freq"'));
  assert.ok(html.includes('data-key="family"'));
  assert.ok(html.includes('data-key="identity"'));
  assert.ok(html.includes('data-key="count"'));
});

test("Selections and Signal inventory sit inside a left sidebar; the live view and controls do not", () => {
  const start = html.indexOf('<aside id="sidebar"');
  assert.ok(start >= 0, "no #sidebar aside in index.html");
  const end = html.indexOf("</aside>", start);
  assert.ok(end > start);
  const region = html.slice(start, end);
  assert.ok(region.includes('id="selections"'), "selections panel not in the sidebar");
  assert.ok(region.includes('id="inventory"'), "inventory panel not in the sidebar");
  assert.ok(!region.includes('id="live"'), "the live waterfall/spectrum must stay in the main area");
  assert.ok(!region.includes('id="controls"'), "the controls panel must stay in the main area");
});
