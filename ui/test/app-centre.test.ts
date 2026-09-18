// **The centre view's control plane, after T-445's cutover** (T-152 wrote this file; ADR-0013
// §4.3, §8).
//
// What this file used to test alongside these: bracket placement, Confirmed band boxes and their
// draggable edges, the DC mask, hover text, drag-to-select, click targets, axis ticks, the history
// grid → waterfall-row mapping, and `decimateRow`. Every one of those was a property of a renderer
// docs/16 §8.5 has retired. The user-visible invariants behind them did not go with them: they are
// re-pointed at the canvas in `ui/test/surface-marks.test.ts` (the boxes, their placement, the
// hover readout, the click target) and `ui/test/surface-cutover.test.ts` (the one ramp, the one
// wheel, the fill rule, the mounts, the clock guard).
//
// What is left here is what was never about a renderer: `view.ts`'s Go-to decision, the view a new
// header produces, and **T-343's control — a gesture cannot reach the front end.** That last one is
// the most valuable test in the repo's UI tier and is unchanged by the cutover, which is itself
// worth noticing: the surface adds a *new* gesture surface and a new offer path (T-444), and the
// caller set of the device routes is still exactly two files.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import * as ax from "../src/axis";
import { ControlError, reactionTo } from "../src/controls/client";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { centreInitial } from "../src/app/centre/slice";
import { applyDeviceAction, centreView, centreViewKey, geometryOfLive, gotoDecision, mayRetune, nextView, NOT_LIVE_TEXT, retuneAction, retuneErrorText, viewHooks } from "../src/app/centre/view";

const G: ax.Geometry = { centerHz: 100_000_000, bandwidthHz: 2_400_000, bins: 1024 };
const V: ax.View = { loHz: 99_000_000, hiHz: 101_000_000 }; // 2 MHz view
const near = (a: number, b: number, eps = 1e-9) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

/** A context whose client records every call, so "reached the device" is observable. */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: { ...s.device, loaded: true, live, deviceId: "hackrf:0000000000000000fake0000000000ab" },
    live: { ...s.live, centerHz: 100e6, bandwidthHz: 2.4e6, bins: 1024, view: { loHz: 99.5e6, hiHz: 100.5e6 } },
  }));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

/** Every `.ts` file under `dir`, as paths relative to ui/ (tests run from there). */
function walk(dir: string): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = `${dir}/${e.name}`;
    if (e.isDirectory()) out.push(...walk(p));
    else if (e.name.endsWith(".ts")) out.push(p);
  }
  return out;
}

test("Go to: pan inside the band keeping width, retune outside when live, else not_live", () => {
  const d = gotoDecision(G, V, 100_900_000, true);
  assert.equal(d?.kind, "pan");
  const full = ax.fullView(G), pv = (d as { view: ax.View }).view;
  near(pv.hiHz - pv.loHz, 2_000_000, 1e-6);
  near(pv.hiHz, full.hiHz, 1e-6); // clamped at the band edge
  assert.deepEqual(gotoDecision(G, V, 145e6, true), { kind: "retune", centerHz: 145e6 });
  assert.deepEqual(gotoDecision(G, V, 145e6, false), { kind: "not_live" });
  assert.deepEqual(gotoDecision(null, null, 145e6, true), { kind: "retune", centerHz: 145e6 });
  assert.equal(gotoDecision(G, V, NaN, true), null);
  assert.equal(mayRetune({ loaded: false, live: false }), true);
  assert.equal(mayRetune({ loaded: true, live: false }), false);
  assert.equal(retuneErrorText(new ControlError(409, "not_live", "replay")), NOT_LIVE_TEXT);
  assert.match(retuneErrorText(new ControlError(400, "invalid", "bad centre")), /refused: bad centre/);
});

test("view after a header: kept, clamped, re-centred after a retune, or the requested one", () => {
  const full = ax.fullView(G);
  assert.deepEqual(nextView(G, null), full);
  const kept = nextView(G, V);
  near(kept.loHz, V.loHz, 1e-6); near(kept.hiHz, V.hiHz, 1e-6);
  const moved = nextView(G, { loHz: 144e6, hiHz: 144.5e6 }); // old tune's view, wholly outside
  near(moved.hiHz - moved.loHz, 500_000, 1e-6); near((moved.loHz + moved.hiHz) / 2, G.centerHz, 1e-6);
  const want = nextView(G, V, { loHz: 100.2e6, hiHz: 100.4e6 });
  near(want.loHz, 100.2e6, 1e-6); near(want.hiHz, 100.4e6, 1e-6);
  assert.deepEqual(geometryOfLive(centreInitial().live), null);
  assert.deepEqual(geometryOfLive({ centerHz: 1, bandwidthHz: 2, bins: 3 }), { centerHz: 1, bandwidthHz: 2, bins: 3 });
  assert.equal(centreInitial().live.pendingView, null);
});

test("T-343 control: panning past the band edge reaches no device route, at any overflow", () => {
  const { ctx, calls } = deviceSpyCtx();
  const hooks = viewHooks(ctx);
  const g = geometryOfLive(ctx.store.get().live)!;

  // A pan that ends far past the edge — twenty times the old 5 % overflow threshold, the case that
  // used to POST /api/control/center on pointerup.
  const v = ctx.store.get().live.view!;
  const w = v.hiHz - v.loHz;
  for (const overflowHz of [0.06 * w, w, 10 * w, -0.06 * w, -w, -10 * w]) {
    const p = ax.panView(g, v, overflowHz);
    hooks.setView(p.view);
    hooks.edgeOffer(ax.panRetuneCenter(p.view, p.overflowHz), { loHz: p.view.loHz + p.overflowHz, hiHz: p.view.hiHz + p.overflowHz });
  }
  assert.deepEqual(calls, [], "a pan must never reach the control API");
  // The view moved and the pan left an offer behind — it is not that the gesture did nothing.
  assert.notDeepEqual(ctx.store.get().live.view, v, "the pan still panned");
  assert.ok(ctx.store.get().live.retuneOffer, "the pan offers a retune instead of performing one");
});

test("T-343: only an explicit device action reaches /api/control/center, and it names the device", async () => {
  const { ctx, calls } = deviceSpyCtx();
  await applyDeviceAction(ctx, retuneAction(99_123_456.7, "edge-offer", { loHz: 98e6, hiHz: 100e6 }));
  assert.deepEqual(calls, [{ method: "POST", path: "/api/control/center", body: { center_hz: 99_123_457 } }]);
  // Accepting the offer clears it, and the view it asked for is pending until the next header.
  assert.equal(ctx.store.get().live.retuneOffer, null);
  assert.deepEqual(ctx.store.get().live.pendingView, { loHz: 98e6, hiHz: 100e6 });
  assert.match(ctx.store.get().toast.text, /Retuning hackrf:.* to 99\.1235 MHz/);
});

test("T-343: a replay offers nothing and reaches nothing — the device is not there to move", async () => {
  const { ctx, calls } = deviceSpyCtx(false);
  viewHooks(ctx).edgeOffer(105e6, { loHz: 104.5e6, hiHz: 105.5e6 });
  assert.equal(ctx.store.get().live.retuneOffer, null, "no offer to press on a replay");
  await applyDeviceAction(ctx, retuneAction(105e6, "goto"));
  assert.deepEqual(calls, [], "not_live is decided before the request");
  assert.equal(ctx.store.get().toast.text, NOT_LIVE_TEXT);
});

test("T-343: a busy radio is reported, never retried into a race", () => {
  const e = new ControlError(409, "device_busy", "the front end (hackrf:abc) is busy: a retune has held it for 3.0 s");
  assert.match(retuneErrorText(e), /^The radio is busy: the front end \(hackrf:abc\) is busy/);
  assert.deepEqual(reactionTo(e).reaction, "busy");
});

test("T-343: gestures.ts names no device route, and the device routes have exactly two callers", () => {
  const DEVICE_ROUTES = ["/api/control/center", "/api/control/rate", "/api/control/gains", "/api/control/bias_tee", "/api/control/baseband_filter"];
  const gestures = readFileSync("src/controls/gestures.ts", "utf8");
  for (const r of DEVICE_ROUTES) assert.ok(!gestures.includes(r), `gestures.ts must not name ${r}`);
  assert.ok(!/controls\/client|ControlClient|app\/context/.test(gestures), "gestures.ts must not reach the API client");

  const callers = walk("src")
    .filter((f) => DEVICE_ROUTES.some((r) => readFileSync(f, "utf8").includes(r)))
    .map((f) => f.slice("src/".length))
    .sort();
  // view.ts: the centre view's one retune path (Go to, a bookmark jump, an accepted edge offer).
  // review/device.ts: the SDR control panel, whose whole purpose is device settings.
  assert.deepEqual(callers, ["app/centre/view.ts", "app/review/device.ts"],
    "a new file reaches the front end: make it an explicit device action or route it through view.ts");
});

test("T-386 THE PROPERTY: with no stream header the view is still placed, in the tuned band the device reports", () => {
  // The original defect: every bracket, band, selection box and frequency tick was placed in
  // `live.view`, which exists only once a spectrum stream header has arrived — so a replay whose
  // stream had not connected drew NOTHING while `/api/inventory` was answering with rows for
  // exactly the band the device reports. `centreView` falls back to the tuned band, and the surface
  // does not need `centreView` at all: its frequency bounds come from `GET /api/tiles`' lattice and
  // `GET /api/navigation`'s ranges, neither of which is a stream. The stream cannot gate the
  // picture any more, because the picture is not made of the stream.
  const noHeader = { live: { view: null }, device: { centerHz: 101.3e6, sampleRateHz: 2e6 } };
  assert.deepEqual(centreView(noHeader), { loHz: 100.3e6, hiHz: 102.3e6 });
});

test("T-386 THE CONTROL: with neither a header nor a device the view stays UNKNOWN, never invented", () => {
  assert.equal(centreView({ live: { view: null }, device: { centerHz: null, sampleRateHz: null } }), null);
  assert.equal(centreView({ live: { view: null }, device: { centerHz: 101.3e6, sampleRateHz: null } }), null);
  // The header still wins when there is one: a zoom is the view, not the whole tuned band.
  assert.deepEqual(
    centreView({ live: { view: { loHz: 101e6, hiHz: 101.5e6 } }, device: { centerHz: 101.3e6, sampleRateHz: 2e6 } }),
    { loHz: 101e6, hiHz: 101.5e6 },
  );
});
