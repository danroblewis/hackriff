// T-822 / MAP-22: where a measure-mode stroke goes. `commitMeasurement` is the whole of the
// decision — which of docs/25 §4's kinds a two-cursor drag supports, and the request it builds —
// mirroring `app-explore.test.ts`'s `commitRegion` tests for the sibling shift+drag gesture.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { AppContext } from "../src/app/context";
import { commitMeasurement, measureIsReal, measureKinds } from "../src/app/explore/measure";
import type { MarkRegion } from "../src/surface/marks";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";

const REGION: MarkRegion = { f0Hz: 101.2e6, f1Hz: 101.4e6, t0Ns: 1_789_300_000e9, t1Ns: 1_789_300_005e9 };
const VIEW = {
  center_hz: 101.3e6, span_hz: 2e6, t_capture: [1_789_299_990, 1_789_300_010] as [number, number],
  tier: "live-iq" as const, device_id: "hackrf-0",
};
const fmt = (r: MarkRegion) => `Δf ${r.f1Hz - r.f0Hz} · Δt ${(r.t1Ns - r.t0Ns) / 1e9}`;

function measureCtx() {
  const store = createStore(initialState());
  const calls: { method: string; path: string; body?: unknown }[] = [];
  let nextId = 0;
  const client = {
    post: async <T>(path: string, body?: unknown): Promise<T> => {
      calls.push({ method: "POST", path, body });
      const b = body as { kind: string; cursors: { f_hz: number; t_s: number }[] };
      const f = b.cursors.map((c) => c.f_hz), t = b.cursors.map((c) => c.t_s);
      return {
        id: `m${nextId++}`, kind: b.kind, f_lo_hz: Math.min(...f), f_hi_hz: Math.max(...f),
        t0_s: Math.min(...t), t1_s: Math.max(...t),
      } as T;
    },
    put: async <T>(): Promise<T> => { throw new Error("measure never PUTs"); },
    del: async <T>(): Promise<T> => { throw new Error("measure never DELETEs"); },
  };
  return { store, calls, ctx: { store, client } as unknown as AppContext };
}

test("measureIsReal: extent on EITHER axis is enough — unlike a region, which needs both", () => {
  assert.equal(measureIsReal(REGION), true);
  assert.equal(measureIsReal({ ...REGION, f1Hz: REGION.f0Hz }), true, "a pure Δt stroke is still real");
  assert.equal(measureIsReal({ ...REGION, t1Ns: REGION.t0Ns }), true, "a pure Δf stroke is still real");
  assert.equal(measureIsReal({ ...REGION, f1Hz: REGION.f0Hz, t1Ns: REGION.t0Ns }), false, "a point is not");
});

test("measureKinds: delta_f and delta_t independently, from the same two cursors", () => {
  assert.deepEqual(measureKinds(REGION), ["delta_f", "delta_t"]);
  assert.deepEqual(measureKinds({ ...REGION, f1Hz: REGION.f0Hz }), ["delta_t"]);
  assert.deepEqual(measureKinds({ ...REGION, t1Ns: REGION.t0Ns }), ["delta_f"]);
  assert.deepEqual(measureKinds({ ...REGION, f1Hz: REGION.f0Hz, t1Ns: REGION.t0Ns }), []);
});

test("commitMeasurement: a diagonal drag POSTs delta_f AND delta_t, sharing the same two cursors and view", async () => {
  const { calls, ctx } = measureCtx();
  const saved = await commitMeasurement(ctx, REGION, VIEW, fmt);
  assert.deepEqual(calls.map((c) => (c.body as { kind: string }).kind), ["delta_f", "delta_t"]);
  const cursors = [
    { f_hz: REGION.f0Hz, t_s: REGION.t0Ns / 1e9 }, { f_hz: REGION.f1Hz, t_s: REGION.t1Ns / 1e9 },
  ];
  for (const c of calls) {
    assert.equal(c.path, "/api/measurements");
    assert.deepEqual((c.body as { cursors: unknown }).cursors, cursors, "no value/unit sent: the server computes it");
    assert.deepEqual((c.body as { view: unknown }).view, VIEW);
  }
  assert.equal(saved.length, 2);
});

test("commitMeasurement: a pure Δt stroke (no frequency travel) saves ONLY delta_t", async () => {
  const { calls, ctx } = measureCtx();
  const vertical = { ...REGION, f1Hz: REGION.f0Hz };
  await commitMeasurement(ctx, vertical, VIEW, fmt);
  assert.deepEqual(calls.map((c) => (c.body as { kind: string }).kind), ["delta_t"]);
});

test("commitMeasurement: a tap (no extent on either axis) commits nothing, and reports nothing", async () => {
  const { calls, store, ctx } = measureCtx();
  const saved = await commitMeasurement(ctx, { ...REGION, f1Hz: REGION.f0Hz, t1Ns: REGION.t0Ns }, VIEW, fmt);
  assert.deepEqual(calls, []);
  assert.deepEqual(saved, []);
  assert.equal(store.get().toast.text, "");
});

test("commitMeasurement: a refused request reports the server's reason, and returns what DID save", async () => {
  const { store, ctx } = measureCtx();
  let n = 0;
  (ctx.client as unknown as { post: unknown }).post = async () => {
    n++;
    if (n === 2) throw { code: "invalid", message: "n must be in 1..=1000000" };
    return { id: "m0", kind: "delta_f", f_lo_hz: REGION.f0Hz, f_hi_hz: REGION.f1Hz, t0_s: REGION.t0Ns / 1e9, t1_s: REGION.t1Ns / 1e9 };
  };
  const saved = await commitMeasurement(ctx, REGION, VIEW, fmt);
  assert.match(store.get().toast.text, /n must be in/);
  assert.equal(saved.length, 1, "the first POST that DID succeed is still reported back");
});
