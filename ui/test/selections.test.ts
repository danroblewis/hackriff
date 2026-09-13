// T-044 multi-region selection model.
import { test } from "node:test";
import assert from "node:assert/strict";
import { SELECTION_ACTIONS, SelectionStore, validateSelection, type Selection } from "../src/selections";

function store() {
  let id = 0, t = 1_789_297_800;
  return new SelectionStore({ newId: () => `sel-${++id}`, now: () => t++ });
}

test("several selections coexist, in creation order, with default names", () => {
  const s = store();
  const a = s.add({ f_lo: 101.2e6, f_hi: 101.4e6 });
  const b = s.add({ f_lo: 99.9e6, f_hi: 100.1e6, t_lo: 10, t_hi: 12.5 });
  const c = s.add({ name: "  pager   burst ", f_lo: 1, f_hi: 2 });
  assert.deepEqual(s.list().map((x) => x.id), ["sel-1", "sel-2", "sel-3"]);
  assert.deepEqual(a, { id: "sel-1", name: "Region 1", f_lo: 101.2e6, f_hi: 101.4e6, created: 1_789_297_800 } satisfies Selection);
  assert.deepEqual({ t_lo: b.t_lo, t_hi: b.t_hi, name: b.name }, { t_lo: 10, t_hi: 12.5, name: "Region 2" });
  assert.equal(c.name, "pager burst");
  assert.equal("t_lo" in a, false, "no time extent unless given");
  assert.equal(s.get("sel-2"), b);
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
});

test("delete and clear; subscribers see every change and can unsubscribe", () => {
  const s = store();
  const seen: number[] = [];
  const off = s.subscribe((list) => seen.push(list.length));
  const a = s.add({ f_lo: 1, f_hi: 2 });
  s.add({ f_lo: 3, f_hi: 4 });
  assert.equal(s.remove(a.id), true);
  assert.equal(s.remove(a.id), false);
  s.rename(s.list()[0].id, "kept");
  s.clear();
  s.clear();
  off();
  s.add({ f_lo: 5, f_hi: 6 });
  assert.deepEqual(seen, [1, 2, 1, 1, 0]);
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

test("demod/record/inspect actions are stubs for T-052", () => {
  assert.deepEqual(SELECTION_ACTIONS.map((a) => a.id), ["inspect", "demod", "record"]);
  assert.ok(SELECTION_ACTIONS.every((a) => a.task === "T-052"));
});
