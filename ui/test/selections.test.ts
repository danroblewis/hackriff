// T-044 multi-region selection model; T-052 server sync, offline fallback and action dispatch.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ControlClient, ControlError } from "../src/controls/client";
import {
  SELECTION_ACTIONS, type NewLink, type Selection, type SelectionActionHooks, type SelectionBackend, SelectionStore,
  apiBackend, inspectQuery, inspectRows, inspectSelection, runSelectionAction, sortSelections, syncText,
  uuid4, validateSelection,
} from "../src/selections";

function store(backend: SelectionBackend | null = null) {
  let id = 0, t = 1_789_297_800;
  return new SelectionStore({ newId: () => `sel-${++id}`, now: () => t++, backend });
}

test("several selections coexist, in creation order, with default names", () => {
  const s = store();
  const a = s.add({ f_lo: 101.2e6, f_hi: 101.4e6 });
  const b = s.add({ f_lo: 99.9e6, f_hi: 100.1e6, t_lo: 10, t_hi: 12.5 });
  const c = s.add({ name: "  pager   burst ", f_lo: 1, f_hi: 2 });
  assert.deepEqual(s.list().map((x) => x.id), ["sel-1", "sel-2", "sel-3"]);
  assert.deepEqual(a, { id: "sel-1", name: "Region 1", f_lo: 101.2e6, f_hi: 101.4e6, tags: [], links: [], created: 1_789_297_800, updated: 1_789_297_800 } satisfies Selection);
  assert.deepEqual({ t_lo: b.t_lo, t_hi: b.t_hi, name: b.name }, { t_lo: 10, t_hi: 12.5, name: "Region 2" });
  assert.equal(c.name, "pager burst");
  assert.equal("t_lo" in a, false, "no time extent unless given");
  assert.equal(s.get("sel-2"), b);
  assert.equal(s.sync().mode, "local");
  assert.equal(syncText(s.sync(), 3), "3 regions (this page only)");
});

test("rename trims, refuses empty names and unknown ids", () => {
  const s = store();
  const a = s.add({ f_lo: 1, f_hi: 2 });
  assert.equal(s.rename(a.id, "  FM 101.3 "), true);
  assert.equal(s.get(a.id)!.name, "FM 101.3");
  assert.equal(s.rename(a.id, "   "), false);
  assert.equal(s.get(a.id)!.name, "FM 101.3");
  assert.equal(s.rename("nope", "x"), false);
  assert.equal(s.rename(a.id, "x".repeat(100)), true);
  assert.equal(s.get(a.id)!.name.length, 64);
  assert.equal(s.edit(a.id, { notes: "RDS", tags: [" fm", "fm", ""] }), true);
  assert.deepEqual({ notes: s.get(a.id)!.notes, tags: s.get(a.id)!.tags }, { notes: "RDS", tags: ["fm"] });
});

test("delete, clear and multi-select; subscribers see every change and can unsubscribe", () => {
  const s = store();
  const seen: number[] = [];
  const off = s.subscribe((list) => seen.push(list.length));
  const a = s.add({ f_lo: 1, f_hi: 2 });
  const b = s.add({ f_lo: 3, f_hi: 4 });
  s.pick(a.id);
  s.pick(b.id, true);
  assert.deepEqual(s.pickedList().map((x) => x.id), [a.id, b.id]);
  assert.equal(s.remove(a.id), true);
  assert.equal(s.remove(a.id), false);
  assert.equal(s.isPicked(a.id), false, "a deleted selection is unticked");
  s.pickAll(false);
  assert.equal(s.pickedList().length, 0);
  s.rename(s.list()[0].id, "kept");
  s.clear();
  s.clear();
  off();
  s.add({ f_lo: 5, f_hi: 6 });
  assert.deepEqual(seen, [1, 2, 2, 2, 1, 1, 1, 0]);
});

test("invalid selections are refused", () => {
  const s = store();
  assert.throws(() => s.add({ f_lo: 2, f_hi: 2 }), /f_lo < f_hi/);
  assert.throws(() => s.add({ f_lo: -1, f_hi: 2 }), /f_lo < f_hi/);
  assert.throws(() => s.add({ f_lo: NaN, f_hi: 2 }), /finite/);
  assert.throws(() => s.add({ f_lo: 1, f_hi: 2, t_lo: 5 }), /both/);
  assert.throws(() => s.add({ f_lo: 1, f_hi: 2, t_lo: 5, t_hi: 4 }), /t_lo <= t_hi/);
  assert.throws(() => s.add({ f_lo: 1, f_hi: 2, name: " " }), /name/);
  assert.equal(validateSelection({ f_lo: 1, f_hi: 2, t_lo: 5, t_hi: 5 }), null);
  assert.equal(s.list().length, 0);
});

test("uuid4 makes RFC 4122 v4 ids (server SelectionId is a UUID)", () => {
  const ids = new Set(Array.from({ length: 50 }, uuid4));
  assert.equal(ids.size, 50);
  for (const id of ids) assert.match(id, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
});

/** An in-memory server with switchable failure. */
class FakeServer implements SelectionBackend {
  rows = new Map<string, Selection>();
  calls: string[] = [];
  fail: ((op: string) => unknown) | null = null;
  private clock = 2_000_000_000;

  private check(op: string) {
    this.calls.push(op);
    const e = this.fail?.(op);
    if (e) throw e;
  }
  async list() { this.check("list"); return [...this.rows.values()]; }
  async create(s: Selection) {
    this.check(`create ${s.id}`);
    if (this.rows.has(s.id)) throw new ControlError(409, "conflict", "exists");
    const saved = { ...s, links: [...s.links.filter(() => false)], updated: this.clock++ };
    this.rows.set(s.id, saved);
    return saved;
  }
  async update(id: string, s: Selection) {
    this.check(`update ${id}`);
    const old = this.rows.get(id);
    if (!old) throw new ControlError(404, "not_found", "no such selection");
    const saved = { ...old, name: s.name, notes: s.notes, tags: s.tags, updated: this.clock++ };
    this.rows.set(id, saved);
    return saved;
  }
  async remove(id: string) {
    this.check(`delete ${id}`);
    if (!this.rows.delete(id)) throw new ControlError(404, "not_found", "no such selection");
  }
  async link(id: string, l: NewLink) {
    this.check(`link ${id} ${l.kind}`);
    const old = this.rows.get(id);
    if (!old) throw new ControlError(404, "not_found", "no such selection");
    const saved = { ...old, links: [...old.links, { kind: l.kind, target: l.target, t: this.clock, note: l.note ?? null }], updated: this.clock++ };
    this.rows.set(id, saved);
    return saved;
  }
}

const offline = () => new TypeError("fetch failed");

test("server-backed: load merges the server list; add, rename, link and delete sync optimistically", async () => {
  const srv = new FakeServer();
  srv.rows.set("srv-1", { id: "srv-1", name: "from server", f_lo: 1e6, f_hi: 2e6, tags: [], links: [], created: 1, updated: 1 });
  const s = store(srv);
  assert.equal(s.sync().mode, "syncing");
  await s.load();
  assert.deepEqual(s.list().map((x) => x.name), ["from server"]);
  assert.equal(s.sync().mode, "synced");

  const a = s.add({ f_lo: 101.2e6, f_hi: 101.4e6 });
  assert.equal(s.list().length, 2, "optimistic: listed before the server answers");
  await s.idle();
  assert.deepEqual(srv.calls.slice(-1), [`create ${a.id}`], "created with the client id");
  assert.equal(s.get(a.id)!.updated, srv.rows.get(a.id)!.updated, "server copy adopted");

  assert.equal(s.rename(a.id, "FM"), true);
  assert.equal(s.get(a.id)!.name, "FM", "optimistic rename");
  s.link(a.id, { kind: "recording", target: "rec-1" });
  assert.equal(s.get(a.id)!.links.length, 1, "optimistic link");
  await s.idle();
  assert.equal(srv.rows.get(a.id)!.name, "FM");
  assert.equal(srv.rows.get(a.id)!.links[0].target, "rec-1");
  assert.equal(s.get(a.id)!.links.length, 1);

  assert.equal(s.remove("srv-1"), true);
  await s.idle();
  assert.equal(srv.rows.has("srv-1"), false);
  assert.deepEqual(s.sync(), { mode: "synced", pending: 0, message: "" });
  assert.match(syncText(s.sync(), 1), /saved on the server/);
});

test("offline fallback: changes stay in the page as pending and sync when the server is back", async () => {
  const srv = new FakeServer();
  srv.fail = offline;
  const s = store(srv);
  await s.load();
  assert.equal(s.sync().mode, "offline");
  const a = s.add({ f_lo: 1e6, f_hi: 2e6 });
  const b = s.add({ f_lo: 3e6, f_hi: 4e6 });
  s.rename(a.id, "made offline");
  s.link(a.id, { kind: "inspection", target: "no-emitters" });
  s.remove(b.id);
  await s.idle();
  assert.deepEqual(s.list().map((x) => x.name), ["made offline"], "nothing lost locally");
  const st = s.sync();
  assert.equal(st.mode, "offline");
  assert.equal(st.pending, 2);
  assert.match(syncText(st, 1), /offline: kept in this page, 2 changes will sync/);

  // The server comes back; a retried create of an id it already has (e.g. a lost response) updates instead.
  srv.fail = null;
  srv.rows.set(a.id, { ...s.get(a.id)!, name: "stale", links: [] });
  await s.flush();
  assert.equal(srv.rows.get(a.id)!.name, "made offline");
  assert.equal(srv.rows.get(a.id)!.links.length, 1, "queued link sent");
  assert.ok(srv.calls.includes(`delete ${b.id}`), "queued delete sent (404 counts as done)");
  assert.deepEqual(s.sync(), { mode: "synced", pending: 0, message: s.sync().message });
  assert.equal(s.sync().pending, 0);

  // A token rejection also keeps changes pending, with a notice.
  srv.fail = (op) => (op.startsWith("update") ? new ControlError(401, "unauthorized", "missing or invalid token") : null);
  s.rename(a.id, "later");
  await s.idle();
  assert.equal(s.sync().mode, "offline");
  assert.match(s.sync().message, /token rejected/);
  srv.fail = null;
  await s.load();
  assert.equal(srv.rows.get(a.id)!.name, "later", "load keeps and flushes local edits");
  assert.equal(s.get(a.id)!.name, "later");
});

test("server refusals roll back with a notice", async () => {
  const srv = new FakeServer();
  const s = store(srv);
  await s.load();
  const a = s.add({ f_lo: 1e6, f_hi: 2e6, name: "good" });
  await s.idle();
  srv.fail = (op) => (op.startsWith("update") ? new ControlError(400, "invalid", "selection name must be 1..=120 characters") : null);
  s.rename(a.id, "rejected");
  await s.idle();
  assert.equal(s.get(a.id)!.name, "good", "rolled back to the server copy");
  assert.match(s.sync().message, /refused by the server: 400/);
  srv.fail = (op) => (op.startsWith("create") ? new ControlError(400, "invalid", "bad") : null);
  const b = s.add({ f_lo: 5e6, f_hi: 6e6 });
  await s.idle();
  assert.equal(s.get(b.id), undefined, "a create the server refused is dropped");
  assert.equal(s.sync().mode, "synced");
});

test("apiBackend sends Bearer requests to /api/selections with the client id", async () => {
  const seen: { url: string; method: string; auth: string; body?: unknown }[] = [];
  const wire = { id: "0190a1b2-c3d4-4e5f-8a6b-7c8d9e0f1a2b", name: "x", f_lo: 1, f_hi: 2, t_lo: null, t_hi: null, notes: null, tags: [], links: [], created: 5, updated: 6 };
  const client = new ControlClient("tok-123", async (url, init) => {
    const headers = init.headers as Record<string, string>;
    seen.push({ url, method: init.method!, auth: headers.Authorization, body: init.body ? JSON.parse(init.body as string) : undefined });
    const data = url === "/api/selections" && init.method === "GET" ? { selections: [wire] } : wire;
    return { ok: true, status: 200, statusText: "OK", json: async () => data };
  });
  const b = apiBackend(client);
  const [one] = await b.list();
  assert.deepEqual(one, { id: wire.id, name: "x", f_lo: 1, f_hi: 2, tags: [], links: [], created: 5, updated: 6 }, "nulls dropped");
  await b.create({ ...one, t_lo: 10, t_hi: 11 });
  await b.create(one);
  await b.update(one.id, { ...one, notes: "n" });
  await b.link(one.id, { kind: "recording", target: "r1" });
  await b.remove(one.id);
  assert.ok(seen.every((r) => r.auth === "Bearer tok-123" && !r.url.includes("token=")));
  assert.deepEqual(seen.map((r) => `${r.method} ${r.url}`), [
    "GET /api/selections", "POST /api/selections", "POST /api/selections", `PUT /api/selections/${wire.id}`,
    `POST /api/selections/${wire.id}/links`, `DELETE /api/selections/${wire.id}`,
  ]);
  assert.deepEqual(seen[1].body, { id: wire.id, name: "x", f_lo: 1, f_hi: 2, t_lo: 10, t_hi: 11, notes: null, tags: [] });
  assert.equal("t_lo" in (seen[2].body as object), false, "untimed create sends no time fields");
  assert.deepEqual(seen[3].body, { name: "x", f_lo: 1, f_hi: 2, t_lo: null, t_hi: null, notes: "n", tags: [] });
  assert.deepEqual(seen[4].body, { kind: "recording", target: "r1" });
});

test("actions: dispatch stores links; listen is synchronous", async () => {
  assert.deepEqual(SELECTION_ACTIONS.map((a) => a.id), ["inspect", "listen", "demod", "record"]);
  const s = store();
  const a = s.add({ f_lo: 101.2e6, f_hi: 101.4e6, name: "FM" });
  const b = s.add({ f_lo: 930.4e6, f_hi: 930.6e6, name: "pager" });
  const log: string[] = [];
  const hooks: SelectionActionHooks = {
    inspect: async (x) => { log.push(`inspect ${x.name}`); return { status: "done", message: "ok", link: { kind: "inspection", target: "em-1" } }; },
    listen: (x) => { log.push(`listen ${x.name}`); },
    demod: async () => ({ status: "unavailable", message: "no server" }),
    record: async (x) => { if (x.name === "pager") throw new Error("boom"); return { status: "done", message: "rec", link: { kind: "recording", target: "rec-9" } }; },
  };
  const p = runSelectionAction("listen", [a, b], s, hooks);
  assert.deepEqual(log, ["listen FM"], "listen runs inside the click gesture, first target only");
  assert.equal((await p)[0].status, "done");
  const insp = await runSelectionAction("inspect", [a, b], s, hooks);
  assert.equal(insp.length, 2);
  assert.deepEqual(s.get(a.id)!.links.map((l) => `${l.kind}:${l.target}`), ["inspection:em-1"]);
  const rec = await runSelectionAction("record", [a, b], s, hooks);
  assert.deepEqual(rec.map((r) => r.status), ["done", "failed"]);
  assert.match(rec[1].message, /pager: boom/);
  assert.equal(s.get(a.id)!.links.at(-1)!.target, "rec-9");
  assert.equal(s.get(b.id)!.links.filter((l) => l.kind === "recording").length, 0);
  const [d] = await runSelectionAction("demod", [a], s, hooks);
  assert.equal(d.status, "unavailable");
});

test("inspect hook: emitters inside the selection, busiest first, with top-k explanations", async () => {
  const sel = { id: "s", name: "FM", f_lo: 101.0e6, f_hi: 101.6e6, t_lo: 100, t_hi: 200, tags: [], links: [], created: 1, updated: 1 };
  assert.equal(inspectQuery(sel), "/api/inventory?f_lo=101000000&f_hi=101600000&t0=100&t1=200&limit=50");
  const ex = (rank: number, label: string) => ({ rank, service: label, label, score: 1 - rank / 10, flags: rank === 1 ? ["off-raster"] : [] });
  const row = (id: string, lo: number, hi: number, count: number, explanations = [ex(2, "NBFM"), ex(1, "FM broadcast"), ex(4, "x"), ex(3, "y")]) =>
    ({ id, f_center_hz: (lo + hi) / 2, bandwidth_hz: hi - lo, f_lo_hz: lo, f_hi_hz: hi, count, last_seen_s: 1, known_status: "known", family: null, explanations });
  const rows = [row("outside", 99e6, 99.2e6, 50), row("quiet", 101.5e6, 101.7e6, 2, []), row("busy", 101.2e6, 101.4e6, 40)];
  const inside = inspectRows(sel, rows);
  assert.deepEqual(inside.map((r) => r.id), ["busy", "quiet"]);
  assert.deepEqual(inside[0].top.map((x) => x.label), ["FM broadcast", "NBFM", "y"]);
  const { outcome, report } = await inspectSelection(async (p) => { assert.match(p, /^\/api\/inventory\?/); return { entries: rows, next_cursor: null }; }, sel);
  assert.equal(report!.emitters.length, 2);
  assert.deepEqual(outcome.status === "done" && outcome.link, { kind: "inspection", target: "busy", note: "2 emitters" });
  const failed = await inspectSelection(async () => { throw new Error("500 inventory query failed"); }, sel);
  assert.equal(failed.outcome.status, "failed");
});

// ---- sidebar sort (T-083): default frequency ascending, header-click toggle ----
// Selections carry no SNR/last-activity/recurrence field, so f_lo is the only sortable column
// (docs: "f_lo or centre for selections").

const bare = (id: string, f_lo: number, f_hi = f_lo + 1e6): Selection => ({ id, name: id, f_lo, f_hi, tags: [], links: [], created: 0, updated: 0 });

test("sortSelections defaults to frequency ascending", () => {
  const list = [bare("a", 200e6), bare("b", 50e6), bare("c", 101e6)];
  assert.deepEqual(sortSelections(list).map((s) => s.id), ["b", "c", "a"]);
  assert.deepEqual(sortSelections(list, 1).map((s) => s.id), ["b", "c", "a"]);
});

test("sortSelections direction toggles (header click again)", () => {
  const list = [bare("a", 200e6), bare("b", 50e6), bare("c", 101e6)];
  assert.deepEqual(sortSelections(list, -1).map((s) => s.id), ["a", "c", "b"]);
});

test("sort direction (kept across a reload) still orders newly-loaded selections the same way", () => {
  // Simulates a header click (dir flips to descending), then the store loading a fresh list from
  // the server; the direction lives outside the list itself, so re-applying it after the reload
  // reproduces the chosen order.
  let dir: 1 | -1 = 1;
  dir = dir === 1 ? -1 : 1;
  assert.equal(dir, -1);
  const reloaded = [bare("x", 10e6), bare("y", 90e6), bare("z", 50e6)];
  assert.deepEqual(sortSelections(reloaded, dir).map((s) => s.id), ["y", "z", "x"]);
});
