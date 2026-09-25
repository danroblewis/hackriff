// T-820 / MAP-20: where an annotation gesture goes (`ui/src/app/explore/annotate.ts`) — the request
// the client BUILDS (CLAUDE.md: "assert the request the client builds", the T-367 lesson), against
// docs/api.md's Annotations contract.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { AppContext } from "../src/app/context";
import {
  LABEL_MAX, annotationsPath, boxRequest, commitAnnotation, fetchAnnotations, normLabel, pointRequest,
} from "../src/app/explore/annotate";
import type { MarkRegion } from "../src/surface/marks";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";

const REGION: MarkRegion = { f0Hz: 101.2e6, f1Hz: 101.4e6, t0Ns: 1_789_300_000e9, t1Ns: 1_789_300_005e9 };
const VIEW = {
  center_hz: 101.3e6, span_hz: 2e6, t_capture: [1_789_299_990, 1_789_300_010] as [number, number],
  tier: "spectrum-history" as const, device_id: "hackrf-0",
};
/** docs/api.md: provenance is the server's — any of these in a body is `400 invalid`. */
const SERVER_ONLY = ["provenance", "author", "actor", "authored", "authored_s", "created_s", "updated_s"];

function spyCtx(fail = false) {
  const store = createStore(initialState());
  const calls: { method: string; path: string; body?: unknown }[] = [];
  const client = {
    get: async <T>(path: string): Promise<T> => {
      calls.push({ method: "GET", path });
      return { annotations: [{ id: "a1", kind: "box", f_lo_hz: 1, f_hi_hz: 2, t0_s: 3, t1_s: 4, label: "x" }] } as T;
    },
    post: async <T>(path: string, body?: unknown): Promise<T> => {
      calls.push({ method: "POST", path, body });
      if (fail) throw new Error("400 invalid");
      return { id: "new", ...(body as object) } as T;
    },
  };
  return { store, calls, ctx: { store, client } as unknown as AppContext };
}

test("normLabel: trimmed, 1–120 characters; cancel/empty/whitespace is no annotation at all", () => {
  assert.equal(normLabel(null), null);
  assert.equal(normLabel(undefined), null);
  assert.equal(normLabel("   "), null);
  assert.equal(normLabel("  hi  "), "hi");
  assert.equal(normLabel("x".repeat(200))!.length, LABEL_MAX);
});

test("boxRequest: kind box, the region in Hz and capture SECONDS, the view — and nothing server-only", () => {
  const b = boxRequest(REGION, "off-raster", VIEW)!;
  assert.deepEqual(b, {
    kind: "box", f_lo_hz: 101.2e6, f_hi_hz: 101.4e6, t0_s: 1_789_300_000, t1_s: 1_789_300_005, label: "off-raster", view: VIEW,
  });
  for (const k of SERVER_ONLY) { assert.ok(!(k in b), k); assert.ok(!(k in b.view), `view.${k}`); }
});

test("boxRequest: a box flat on either axis is never sent (the store refuses it)", () => {
  assert.equal(boxRequest({ ...REGION, f1Hz: REGION.f0Hz }, "x", VIEW), null);
  assert.equal(boxRequest({ ...REGION, t1Ns: REGION.t0Ns }, "x", VIEW), null);
});

test("pointRequest: a text note or marker is a zero-area point at the tapped (Hz, capture instant)", () => {
  const m = pointRequest("marker", { fHz: 433.92e6, tNs: 1_789_300_002.5e9 }, "remote", VIEW);
  assert.deepEqual(m, {
    kind: "marker", f_lo_hz: 433.92e6, f_hi_hz: 433.92e6, t0_s: 1_789_300_002.5, t1_s: 1_789_300_002.5, label: "remote", view: VIEW,
  });
  assert.equal(pointRequest("text", { fHz: 1, tNs: 2e9 }, "n", VIEW).kind, "text");
});

test("annotationsPath: the REQUIRED window (f_lo, f_hi, t0, t1) and the route's max limit", () => {
  const p = annotationsPath({ f0Hz: 100e6, f1Hz: 110e6, t0S: 1_789_300_000, t1S: 1_789_300_100 });
  const u = new URL(p, "http://x");
  assert.equal(u.pathname, "/api/annotations");
  assert.equal(u.searchParams.get("f_lo"), "100000000");
  assert.equal(u.searchParams.get("f_hi"), "110000000");
  assert.equal(u.searchParams.get("t0"), "1789300000");
  assert.equal(u.searchParams.get("t1"), "1789300100");
  assert.equal(u.searchParams.get("limit"), "2000");
});

test("commitAnnotation: exactly one POST /api/annotations with the built body; no device route", async () => {
  const { calls, ctx, store } = spyCtx();
  const body = boxRequest(REGION, "off-raster", VIEW)!;
  const saved = await commitAnnotation(ctx, body);
  assert.deepEqual(calls, [{ method: "POST", path: "/api/annotations", body }]);
  assert.equal(saved?.id, "new");
  assert.match(JSON.stringify(store.get()), /Annotated \(box\): off-raster/);
});

test("commitAnnotation: a refused write draws nothing and says why", async () => {
  const { ctx, store } = spyCtx(true);
  assert.equal(await commitAnnotation(ctx, pointRequest("text", { fHz: 1, tNs: 1e9 }, "n", VIEW)), null);
  assert.match(JSON.stringify(store.get()), /Annotate: /);
});

test("fetchAnnotations: one windowed GET — what makes an annotation visible after a reload", async () => {
  const { calls, ctx } = spyCtx();
  const got = await fetchAnnotations(ctx, { f0Hz: 1, f1Hz: 2, t0S: 3, t1S: 4 });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].method, "GET");
  assert.ok(calls[0].path.startsWith("/api/annotations?"));
  assert.deepEqual(got.map((a) => a.id), ["a1"]);
});
