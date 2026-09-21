// T-149 (ADR-0013 §3): the MUI store contract and the pure state actions.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createStore } from "../src/app/store";
import {
  cycleTheme, focusSignal, goLive, initialState, openReview, parsePrefs, toggleReview, removeOutput, requestGoto, reviewAt, setMode, upsertOutput,
  type OutputEntry,
} from "../src/app/state";
import type { Row } from "../src/inventory";

test("set merges a top-level patch and notifies with (state, prev)", () => {
  const s = createStore({ a: 1, b: { x: 1 } });
  const seen: [number, number][] = [];
  s.subscribe((st, prev) => seen.push([st.a, prev.a]));
  s.set({ a: 2 });
  s.set((st) => ({ a: st.a + 1 }));
  assert.deepEqual(seen, [[2, 1], [3, 2]]);
  assert.deepEqual(s.get().b, { x: 1 });
});

test("a patch that changes nothing does not notify", () => {
  const b = { x: 1 };
  const s = createStore({ a: 1, b });
  let n = 0;
  s.subscribe(() => n++);
  s.set({ a: 1, b });
  s.set({});
  assert.equal(n, 0);
});

test("select fires only when the selected value changes; immediate and unsubscribe work", () => {
  const s = createStore({ a: 1, b: 1 });
  const seen: [number, number | undefined][] = [];
  const off = s.select((st) => st.a, (v, p) => seen.push([v, p]), { immediate: true });
  s.set({ b: 2 });
  s.set({ a: 5 });
  off();
  s.set({ a: 6 });
  assert.deepEqual(seen, [[1, undefined], [5, 1]]);
});

test("select honours a custom equality", () => {
  const s = createStore({ v: { lo: 1, hi: 2 } });
  let n = 0;
  s.select((st) => st.v, () => n++, { eq: (a, b) => a.lo === b.lo && a.hi === b.hi });
  s.set({ v: { lo: 1, hi: 2 } });
  assert.equal(n, 0);
  s.set({ v: { lo: 1, hi: 3 } });
  assert.equal(n, 1);
});

test("set from inside a listener is queued, so every listener sees consistent pairs", () => {
  const s = createStore({ a: 0, b: 0 });
  const pairs: string[] = [];
  s.subscribe((st) => { if (st.a === 1 && st.b === 0) s.set({ b: 1 }); });
  s.subscribe((st, prev) => pairs.push(`${prev.a}${prev.b}->${st.a}${st.b}`));
  s.set({ a: 1 });
  assert.deepEqual(pairs, ["00->10", "10->11"]);
  assert.deepEqual(s.get(), { a: 1, b: 1 });
});

test("a throwing listener is logged by name and the others still receive every update", () => {
  const s = createStore({ a: 0, b: 0 });
  const errors: unknown[][] = [];
  const orig = console.error;
  console.error = (...args: unknown[]) => { errors.push(args); };
  try {
    const seen: string[] = [];
    s.subscribe(function brokenPanel() { throw new Error("boom"); });
    s.select((st) => st.a, () => { throw new Error("select boom"); });
    s.subscribe((st) => { if (st.a === 1 && st.b === 0) s.set({ b: 1 }); }); // queues a patch
    s.subscribe((st, prev) => seen.push(`${prev.a}${prev.b}->${st.a}${st.b}`));
    s.set({ a: 1 });
    assert.deepEqual(seen, ["00->10", "10->11"], "queued patch flushed despite the throwing listeners");
    assert.deepEqual(s.get(), { a: 1, b: 1 });
    assert.equal(errors.length, 3, "brokenPanel twice (both passes), the select listener once");
    assert.match(String(errors[0][0]), /brokenPanel/);
    assert.match(String(errors[1][0]), /anonymous select/);
    s.set({ a: 2 });
    assert.equal(seen.at(-1), "11->21", "the store keeps working afterwards");
  } finally {
    console.error = orig;
  }
});

test("a throwing patch function still lets queued patches flush, then reaches the caller", () => {
  const s = createStore({ a: 0, b: 0 });
  s.subscribe((st) => { if (st.a === 1) s.set(() => { throw new Error("bad patch"); }); });
  s.subscribe((st) => { if (st.a === 1 && st.b === 0) s.set({ b: 1 }); });
  assert.throws(() => s.set({ a: 1 }), /bad patch/);
  assert.deepEqual(s.get(), { a: 1, b: 1 });
});

test("initialState composes every area slice", () => {
  const st = initialState();
  for (const k of ["mode", "theme", "conn", "device", "nav", "toast", "time", "outputs", "focus", "inventory", "selections", "live", "decode", "inspector", "review"]) {
    assert.ok(k in st, `missing ${k}`);
  }
  assert.deepEqual(st.decode, { pipelineId: null, nodeId: null });
  assert.deepEqual(st.inspector, { frameSeq: null, fieldNodeId: null });
});

test("prefs parse defensively", () => {
  assert.deepEqual(parsePrefs(null), { mode: "explore", theme: "system" });
  assert.deepEqual(parsePrefs("{bad"), { mode: "explore", theme: "system" });
  assert.deepEqual(parsePrefs('{"mode":"decode","theme":"light"}'), { mode: "decode", theme: "light" });
  assert.deepEqual(parsePrefs('{"mode":"x","theme":"neon"}'), { mode: "explore", theme: "system" });
  // T-506: the Capture panel is gone, and so is its collapse pref. A blob saved before the removal
  // still parses — the stale key is dropped, not an error and not carried forward.
  assert.deepEqual(parsePrefs('{"mode":"decode","captureCollapsed":true}'), { mode: "decode", theme: "system" });
});

test("mode, theme, time cursor and goto actions", () => {
  const s = createStore(initialState());
  s.set(setMode("decode"));
  assert.equal(s.get().mode, "decode");
  s.set(cycleTheme); s.set(cycleTheme); s.set(cycleTheme);
  assert.equal(s.get().theme, "system");
  s.set(reviewAt(1_789_300_000));
  // T-340: `spanS` is the time span a region dragged on the time navigator asked for; null here
  // means none was asked for, never a default duration.
  assert.deepEqual(s.get().time, { live: false, tS: 1_789_300_000, spanS: null });
  s.set(reviewAt(1_789_300_000, 400));
  assert.deepEqual(s.get().time, { live: false, tS: 1_789_300_000, spanS: 400 });
  s.set(reviewAt(NaN));
  assert.deepEqual(s.get().time, { live: false, tS: 1_789_300_000, spanS: 400 });
  s.set(goLive);
  assert.deepEqual(s.get().time, { live: true });
  s.set(requestGoto(101.3e6));
  s.set(requestGoto(101.3e6));
  assert.deepEqual(s.get().nav, { gotoHz: 101.3e6, seq: 2 });
});

test("review drawer toggles and opens on a tab and region", () => {
  const s = createStore(initialState());
  s.set(toggleReview);
  assert.equal(s.get().review.open, true);
  s.set(openReview("history", { loHz: 101e6, hiHz: 102e6 }));
  assert.deepEqual(s.get().review, { open: true, tab: "history", region: { loHz: 101e6, hiHz: 102e6 } });
});

test("focusing a signal switches the inventory tab to the row's state", () => {
  const s = createStore(initialState());
  const row = { id: "c1", state: "candidate" } as Row;
  s.set((st) => ({ inventory: { ...st.inventory, rows: { c1: row } } }));
  s.set(focusSignal("c1"));
  assert.deepEqual(s.get().focus, { kind: "signal", id: "c1" });
  assert.equal(s.get().inventory.tab, "candidate");
  s.set(focusSignal("unknown"));
  assert.equal(s.get().inventory.tab, "candidate");
});

test("outputs upsert by id and remove", () => {
  const s = createStore(initialState());
  const e: OutputEntry = { id: "o1", kind: "audio", label: "101.300", sub: "audio", state: "opening", tcpTarget: null, muted: false, levelDbfs: null, recordsPerS: null, emitterId: "e1", pipelineId: null, message: null };
  s.set(upsertOutput(e));
  s.set(upsertOutput({ ...e, state: "live" }));
  assert.equal(s.get().outputs.length, 1);
  assert.equal(s.get().outputs[0].state, "live");
  const before = s.get();
  s.set(removeOutput("nope"));
  assert.equal(s.get(), before);
  s.set(removeOutput("o1"));
  assert.equal(s.get().outputs.length, 0);
});
