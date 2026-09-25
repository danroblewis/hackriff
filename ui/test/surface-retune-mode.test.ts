// **T-1028: retune mode — the ONE state in which a gesture commands the radio, and the proof that
// nothing else changed.**
//
// The user amended a stated invariant on 2026-09-25 (root `CLAUDE.md`, docs/23 §4/§10.4, ADR-0023):
//
// > *"the 'retune' functionality would be cool as a mode … whenever they zoom or pan it retunes to
// > that. For areas that are too large and can't be tuned, use the largest possible size instead of
// > denying them … Maybe it's a keyboard thing, hold down a certain key while panning to retune."*
//
// An amendment to a **safety** invariant is only as good as the control that still holds the old
// rule, so this file is written as a pair. Every device-reaching claim below is matched by the same
// gesture with the mode OFF asserting an **empty call list** — T-340's control, T-442's whole-pane
// vocabulary and T-458's region strokes are unchanged and still run in their own files; this one adds
// the same shape of assertion around the new state.
//
// What is asserted, by the ticket's own list:
//
//  1. mode off → pan/zoom reach zero device routes (and `input.ts` still reports nothing itself);
//  2. mode on → ONE settled gesture is ONE snapped retune, its `(centre, span)` asserted by value;
//  3. three quick pans → ONE call, for the LAST view (and an in-flight request is never cancelled);
//  4. a 40 MHz view at 100 MHz on a 20 MHz front end → ONE call for the widest window centred at
//     100 MHz, and **no error surfaced** — the user's "use the largest possible size";
//  5. the held-key form: calls only while held, and release restores mode-off behaviour;
//  6. a FROZEN pane → a frequency retune with its time window untouched.
//
// Plus the structural guards: the mode names no route of its own, and the only view that can settle
// into a refusal is one no configuration reaches at all.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fitToLiveWindow, largestLiveSpan, type FrequencyGrid } from "../src/navigation";
import { attachSurfaceInput } from "../src/surface/input";
import type { SurfacePreview } from "../src/surface/preview";
import { PaneModel } from "../src/surface/panes";
import {
  commitRetuneMode, isRetuneKey, retuneModeAcceptable, retuneModeAction, retuneModeLabel, retuneModeTarget,
  RetuneModeController, RETUNE_SETTLE_MS, type RetuneModeTarget, type RetuneTimers,
} from "../src/surface/retune-mode";
import { isTypingTarget } from "../src/app/centre/active-pane";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 120 * S, t1Ns: T0 };
const STEP = 30e6 / 2 ** 20; // HACKRF_ONE_TUNING_STEP_HZ, ≈ 28.6102294921875 Hz

/** The achievable (centre, span) grid a HackRF-class front end reports on `GET /api/navigation`. */
const GRID: FrequencyGrid = {
  device_id: "hackrf:0000000000000000fake0000000000ab", driver: "hackrf-one", controllable: true,
  ranges_hz: [[1e6, 6e9]], center_step: "uniform", center_step_hz: STEP,
  spans_hz: { min: 2e6, max: 20e6 }, max_live_span_hz: 20e6,
  current: { center_hz: 100e6, span_hz: 2.4e6 },
};

const model = (spanHz = 1e6, centerHz = 100.8e6) => new PaneModel({
  bounds: BOUNDS, width: 1200, height: 800, freq: { centerHz, spanHz }, spanNs: 20 * S,
});

/** A context whose client records every call, so "reached the device" is observable (T-343). */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: {
      ...s.device, loaded: true, live, deviceId: GRID.device_id, sampleRateHz: 2_400_000, centerHz: 100.8e6,
      centerGrid: { ranges_hz: GRID.ranges_hz, center_step_hz: GRID.center_step_hz },
    },
  }));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls, store };
}

/** A settle timer under the test's control: nothing here waits on a wall clock. */
function fakeTimers(): RetuneTimers & { run(): void; armed: number } {
  let queue: { id: number; fn: () => void }[] = [];
  let next = 1;
  const t = {
    set(fn: () => void) { const id = next++; queue.push({ id, fn }); return id; },
    clear(h: unknown) { queue = queue.filter((q) => q.id !== h); },
    /** Fire every timer currently armed (a settle interval elapsing). */
    run() { const q = queue; queue = []; for (const e of q) e.fn(); },
    get armed() { return queue.length; },
  };
  return t as RetuneTimers & { run(): void; armed: number };
}

/** Let the commit's promise chain settle. */
const flush = async () => { for (let i = 0; i < 8; i++) await Promise.resolve(); };

/**
 * The whole mode over a real `PaneModel`, a real `commitRetuneMode` and a spy client — the only
 * thing faked is the clock. `targetNow` re-derives from the live model, which is what makes "the
 * latest settled view wins" a property rather than a restatement of the test's own bookkeeping.
 */
function rig(m: PaneModel, { live = true, edgeNs = T0, grid = GRID as FrequencyGrid | null } = {}) {
  const spy = deviceSpyCtx(live);
  const timers = fakeTimers();
  const invalidations: number[] = [];
  const targetNow = (id: string): RetuneModeTarget | null => {
    const p = m.get(id);
    return p ? retuneModeTarget(p, grid, edgeNs) : null;
  };
  const committed: string[] = [];
  const mode = new RetuneModeController({
    commit: async (paneId) => {
      committed.push(paneId);
      await commitRetuneMode(spy.ctx, {
        targetNow,
        invalidateEdge: () => { invalidations.push(1); return invalidations.length; },
      }, paneId);
    },
    timers,
  });
  return { ...spy, timers, mode, targetNow, committed, invalidations };
}

/** Every POST the run made to a device route. */
const deviceCalls = (calls: { method: string; path: string; body: unknown }[]) =>
  calls.filter((c) => /^\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)$/.test(c.path));

// ---------------------------------------------------------------------------
// 1. THE CONTROL: with the mode off, a pan is still a pan
// ---------------------------------------------------------------------------

test("mode OFF: pans, zooms and their settles reach NO device route — the old rule, verbatim", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  assert.equal(r.mode.on, false, "retune mode is OFF at construction: the amendment is opt-in");
  for (const frac of [0.001, 0.05, 0.5, 1, -0.5, -1]) {
    m.panFreq(id, frac * (BOUNDS.f1Hz - BOUNDS.f0Hz));
    r.mode.moved(id);
  }
  m.zoomFreq(id, 0.25, 0.5);
  r.mode.moved(id);
  r.mode.settled(id); // the gesture ended — and still nothing may go out
  r.timers.run();
  await flush();
  assert.deepEqual(r.calls, [], "a gesture reached the front end with retune mode off");
  assert.equal(r.timers.armed, 0, "mode off must not even arm a settle timer");
});

test("THE PROOF THE CONTROL IS NOT VACUOUS: the same gesture with the mode ON does reach the device", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  r.mode.setSticky(true);
  m.panFreq(id, 3e6);
  r.mode.moved(id);
  r.mode.settled(id);
  await flush();
  assert.equal(deviceCalls(r.calls).length, 1, "mode on: a settled pan must command exactly one retune");
});

// ---------------------------------------------------------------------------
// 2. ONE settled gesture is ONE snapped retune
// ---------------------------------------------------------------------------

test("mode ON: one gesture is ONE window post, at the snapped centre and a covering span", async () => {
  const m = model(1e6, 100.8e6);
  const r = rig(m);
  const id = m.list()[0].id;
  r.mode.setSticky(true);
  m.panFreq(id, 2e6); // the view is now 101.3–102.3 MHz
  r.mode.moved(id);
  r.mode.moved(id);
  r.mode.settled(id);
  await flush();

  const posts = deviceCalls(r.calls);
  assert.equal(posts.length, 1, "one gesture, one device action");
  // T-529: a window is ONE request carrying both halves, never a rate post then a centre post.
  assert.equal(posts[0].path, "/api/control/window");
  const body = posts[0].body as { center_hz: number; sample_rate_hz: number };
  // The centre is on the front end's own synthesiser grid (T-341), never a bare rounded hertz —
  // and it is deliberately NOT the view's own centre: `retunePlan`'s T-418 dodge places the view
  // off the tuner's DC spike, which is the planner doing its job through the mode unchanged.
  assert.ok(Math.abs(body.center_hz / STEP - Math.round(body.center_hz / STEP)) < 1e-6,
    `centre ${body.center_hz} is off the ${STEP} Hz tuning grid`);
  // The window covers the view the gesture came to rest on.
  const pane = m.get(id)!;
  const lo = pane.freq.centerHz - pane.freq.spanHz / 2, hi = pane.freq.centerHz + pane.freq.spanHz / 2;
  assert.ok(body.center_hz - body.sample_rate_hz / 2 <= lo + 1 && body.center_hz + body.sample_rate_hz / 2 >= hi - 1,
    `the window ${body.center_hz}±${body.sample_rate_hz / 2} does not cover the view ${lo}..${hi}`);
  // And the growing edge's tiles, which described the tuning that has just ended, were dropped.
  assert.equal(r.invalidations.length, 1, "T-437 §5.2: the edge must be invalidated after a real retune");
});

test("the action is its own audit source: `retune-mode`, never `pane-offer`", () => {
  const m = model();
  const t = retuneModeTarget(m.list()[0], GRID, T0);
  const a = retuneModeAction(t)!;
  assert.equal(a.source, "retune-mode", "a gesture-driven retune must be tellable from a pressed one");
  assert.equal(a.kind, "retune");
  assert.equal(a.want, null, "the view IS the request: there is no pre-gesture view to restore");
});

// ---------------------------------------------------------------------------
// 3. The latest settled view wins, and one request at a time
// ---------------------------------------------------------------------------

test("three quick pans are ONE retune, and it is for the LAST view", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  r.mode.setSticky(true);
  // Three strokes, none of which was allowed to settle: each re-arms the stillness timer.
  for (const d of [1e6, 2e6, 4e6]) { m.panFreq(id, d); r.mode.moved(id); }
  assert.deepEqual(deviceCalls(r.calls), [], "nothing goes out mid-gesture");
  r.timers.run(); // the view finally stood still
  await flush();
  const posts = deviceCalls(r.calls);
  assert.equal(posts.length, 1, "three pans must not be three retunes");
  const body = posts[0].body as { center_hz: number; sample_rate_hz: number };
  const covers = (loHz: number, hiHz: number) =>
    body.center_hz - body.sample_rate_hz / 2 <= loHz + 1 && body.center_hz + body.sample_rate_hz / 2 >= hiHz - 1;
  const pane = m.get(id)!;
  // Where the view ENDED (100.8 + 7 MHz), not where a stroke passed through (100.8 + 1 MHz). The
  // window is compared rather than the centre, because `retunePlan`'s DC dodge puts the view
  // deliberately off the centre — see the previous test.
  assert.ok(covers(pane.freq.centerHz - pane.freq.spanHz / 2, pane.freq.centerHz + pane.freq.spanHz / 2),
    `the window ${body.center_hz}±${body.sample_rate_hz / 2} does not cover the view the gesture ended on`);
  assert.ok(!covers(101.3e6, 102.3e6), "the retune named a view the gesture only passed through");
});

test("a retune IN FLIGHT is never cancelled: the next settled view waits, then goes out once", async () => {
  const m = model();
  const spy = deviceSpyCtx();
  const timers = fakeTimers();
  // A commit that does not resolve until the test lets it — the in-flight window, held open.
  let release: (() => void) | null = null;
  const inflight: string[] = [];
  const mode = new RetuneModeController({
    commit: (paneId) => { inflight.push(paneId); return new Promise<void>((res) => { release = () => res(); }); },
    timers,
  });
  const id = m.list()[0].id;
  mode.setSticky(true);
  mode.moved(id);
  mode.settled(id);
  assert.deepEqual(inflight, [id], "the first settle went out");
  assert.equal(mode.inFlight, true);
  // Two more settles while it is out: they queue, they do not cancel and they do not stack.
  m.panFreq(id, 5e6); mode.moved(id); mode.settled(id);
  m.panFreq(id, 5e6); mode.moved(id); mode.settled(id);
  assert.deepEqual(inflight, [id], "a request already at the front end must not be joined by another");
  release!();
  await flush();
  // The queued view waits the settle interval again rather than firing the instant the device answers.
  assert.equal(inflight.length, 1, "the queued view must wait for the settle gap, not race the answer");
  timers.run();
  await flush();
  assert.deepEqual(inflight, [id, id], "exactly one follow-up, for the latest view");
  void spy;
});

// ---------------------------------------------------------------------------
// 4. Too wide is NOT an error in the mode (the user's ruling)
// ---------------------------------------------------------------------------

test("a 40 MHz view at 100 MHz on a 20 MHz front end tunes the WIDEST window centred on it, with no error", async () => {
  const m = model(40e6, 100e6);
  const r = rig(m);
  const id = m.list()[0].id;
  const before = { ...m.get(id)!.freq };
  r.mode.setSticky(true);
  r.mode.moved(id);
  r.mode.settled(id);
  await flush();

  const posts = deviceCalls(r.calls);
  assert.equal(posts.length, 1, "a too-wide view must RETUNE, not refuse");
  const body = posts[0].body as { center_hz: number; sample_rate_hz: number };
  assert.equal(body.sample_rate_hz, 20e6, "the largest achievable span, not the view's 40 MHz");
  assert.ok(Math.abs(body.center_hz - 100e6) < STEP, `centred on the view (100 MHz), got ${body.center_hz}`);
  // No error was surfaced: the toast names a retune, never a refusal.
  const toastText = r.store.get().toast?.text ?? "";
  assert.match(toastText, /Retuning/, `a too-wide view surfaced "${toastText}" instead of retuning`);
  assert.doesNotMatch(toastText, /refus|cannot|Outside/i);
  // And the VIEW did not move: the pane still shows the 40 MHz the user framed, with the coverage
  // fog left to say which 20 MHz of it the radio took.
  assert.deepEqual({ ...m.get(id)!.freq }, before, "a retune must not re-frame the viewport");
});

test("the narrowing is SAID, so a user never has to infer it from the fog", () => {
  const m = model(40e6, 100e6);
  const t = retuneModeTarget(m.list()[0], GRID, T0);
  assert.equal(t.narrowed, true);
  assert.equal(t.viewHiHz - t.viewLoHz, 40e6, "the view is carried unnarrowed");
  assert.equal(t.hiHz - t.loHz, 20e6, "the planned region is the widest achievable window");
  const label = retuneModeLabel(t);
  assert.match(label, /20\.000 MHz span/);
  assert.match(label, /widest capture/);
  assert.match(label, /40\.000 MHz you are viewing/);
});

test("the refusal that REMAINS: a view with no overlap with the tunable range reaches nothing", async () => {
  // A front end that tunes 1 MHz–1 GHz, and a view parked at 2 GHz — spectrum the surface can show
  // (the canvas is the whole device-class range) and this radio cannot reach. No clamp gets there,
  // and the user kept this one as an error.
  const narrow: FrequencyGrid = { ...GRID, ranges_hz: [[1e6, 1e9]] };
  const m = model(100e6, 2e9);
  const r = rig(m, { grid: narrow });
  const id = m.list()[0].id;
  r.mode.setSticky(true);
  r.mode.moved(id);
  r.mode.settled(id);
  await flush();
  assert.deepEqual(deviceCalls(r.calls), [], "an unreachable view must not command the radio");
  const t = r.targetNow(id)!;
  assert.equal(retuneModeAcceptable(t), false);
  assert.match(retuneModeLabel(t), /outside the front end's tunable range/);
});

test("a view straddling the top of the band is CLAMPED into it, not refused", () => {
  // 5.95–6.05 GHz: the centre (6.0 GHz) is exactly the top, and the overlap is real.
  const fit = fitToLiveWindow(GRID, 5.95e9, 6.05e9)!;
  assert.ok(fit, "a view overlapping the band must fit");
  const centre = (fit.loHz + fit.hiHz) / 2;
  assert.ok(centre <= 6e9 && centre >= 5.95e9, `clamped centre ${centre} left the band`);
  assert.equal(fit.hiHz - fit.loHz, 20e6, "the widest achievable window");
});

test("fitToLiveWindow leaves a capturable view ALONE — there is one planner, not two", () => {
  const fit = fitToLiveWindow(GRID, 100e6, 102e6)!;
  assert.deepEqual(fit, { loHz: 100e6, hiHz: 102e6 });
  assert.equal(largestLiveSpan(GRID), 20e6);
  // A discrete rate ladder takes its largest entry inside the live bound, never past it.
  const ladder: FrequencyGrid = { ...GRID, spans_hz: { values: [2e6, 8e6, 10e6, 40e6] }, max_live_span_hz: 10e6 };
  assert.equal(largestLiveSpan(ladder), 10e6);
  // No grid, no claim (the `no_grid` refusal, unchanged).
  assert.equal(largestLiveSpan(null), null);
  assert.equal(fitToLiveWindow(null, 100e6, 102e6), null);
});

// ---------------------------------------------------------------------------
// 5. The held-key form
// ---------------------------------------------------------------------------

test("held `R`: gestures command the radio only WHILE it is down; release restores mode-off behaviour", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  // Before the key: the control.
  m.panFreq(id, 1e6); r.mode.moved(id); r.mode.settled(id);
  await flush();
  assert.deepEqual(deviceCalls(r.calls), [], "no key, no mode, no call");

  r.mode.keyDown();
  assert.equal(r.mode.on, true);
  assert.equal(r.mode.latched, false, "a held key must not latch the chip");
  m.panFreq(id, 1e6); r.mode.moved(id); r.mode.settled(id);
  await flush();
  assert.equal(deviceCalls(r.calls).length, 1, "held: the settled gesture tunes");

  r.mode.keyUp();
  assert.equal(r.mode.on, false, "release ends the momentary mode");
  assert.equal(r.mode.latched, false, "a hold that was USED must not also toggle the sticky mode");
  m.panFreq(id, 1e6); r.mode.moved(id); r.mode.settled(id);
  await flush();
  assert.equal(deviceCalls(r.calls).length, 1, "after release, a gesture reaches nothing again");
});

test("a TAP of `R` latches the mode, and a second tap clears it", () => {
  const m = model();
  const r = rig(m);
  r.mode.keyDown();
  r.mode.keyUp(); // nothing happened while it was down: that is a tap
  assert.equal(r.mode.latched, true, "tap latches");
  assert.equal(r.mode.on, true);
  r.mode.keyDown();
  r.mode.keyUp();
  assert.equal(r.mode.latched, false, "a second tap turns it off");
  assert.equal(r.mode.on, false);
});

test("letting go mid-settle abandons the pending retune: releasing the key must not tune a moment later", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  r.mode.keyDown();
  m.panFreq(id, 2e6);
  r.mode.moved(id); // a wheel-shaped gesture: armed, waiting for stillness
  assert.equal(r.timers.armed, 1);
  r.mode.keyUp();
  r.timers.run();
  await flush();
  assert.deepEqual(deviceCalls(r.calls), [], "a released key must not leave a retune in flight behind it");
});

test("`R` is the key, and it never steals a modifier chord or a keystroke being typed", () => {
  const typing = (t: unknown) => isTypingTarget(t);
  assert.equal(isRetuneKey({ key: "r" }, typing), true);
  assert.equal(isRetuneKey({ key: "R" }, typing), true);
  // T-456 spends shift (frequency-only zoom / region), alt (time-only zoom) and ctrl/cmd (uniform
  // zoom, page zoom): a chord must fall through to whoever owns it.
  assert.equal(isRetuneKey({ key: "r", ctrlKey: true }, typing), false);
  assert.equal(isRetuneKey({ key: "r", metaKey: true }, typing), false);
  assert.equal(isRetuneKey({ key: "r", altKey: true }, typing), false);
  // Shift+R is still the mode's key: shift is a modifier on a DRAG/WHEEL here, not on this key, and
  // a capital R typed at the map is the same request as a small one.
  assert.equal(isRetuneKey({ key: "R", shiftKey: true }, typing), true);
  assert.equal(isRetuneKey({ key: "r", target: { tagName: "INPUT", type: "text" } }, typing), false,
    "typing an r into Go-to must never arm a mode that tunes the radio");
  assert.equal(isRetuneKey({ key: "l" }, typing), false, "L is the follow-live key (T-1000)");
  assert.equal(isRetuneKey({ key: "r", defaultPrevented: true }, typing), false);
});

// ---------------------------------------------------------------------------
// 6. Frequency only: a frozen pane stays frozen, and time is never touched
// ---------------------------------------------------------------------------

test("a FROZEN pane in retune mode retunes frequency and stays frozen in time", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  m.pause(id, T0);
  m.panTime(id, -30 * S);
  const time = { ...m.get(id)!.time };
  assert.equal(time.live, false, "the pane is frozen for this test to mean anything");

  r.mode.setSticky(true);
  m.panFreq(id, 3e6);
  r.mode.moved(id);
  r.mode.settled(id);
  await flush();

  assert.equal(deviceCalls(r.calls).length, 1, "a frozen pane still retunes: the mode's user asked for it");
  assert.deepEqual({ ...m.get(id)!.time }, time, "the retune must not move the pane in time");
  // …and the pane says so, rather than leaving the user to wonder why the past did not change.
  const t = r.targetNow(id)!;
  assert.equal(t.frozen, true);
  assert.match(retuneModeLabel(t), /stays frozen/);
});

test("every call this mode can make is a RETUNE: no pause, no ring, no detection route is reachable", async () => {
  const m = model();
  const r = rig(m);
  const id = m.list()[0].id;
  r.mode.setSticky(true);
  for (const d of [1e6, -2e6, 8e6]) { m.panFreq(id, d); r.mode.moved(id); r.mode.settled(id); await flush(); }
  const paths = new Set(r.calls.map((c) => c.path));
  assert.deepEqual([...paths], ["/api/control/window"], "the mode reached a route other than the one window post");
});

// ---------------------------------------------------------------------------
// 7. The gesture wiring: what `input.ts` reports, and what it still does not
// ---------------------------------------------------------------------------

/** A spy canvas + preview: `input.ts` is arithmetic over `PaneModel`, and reaches nothing itself. */
function inputHarness(opts: Parameters<typeof attachSurfaceInput>[2] = {}) {
  const W = 1000, H = 600;
  const listeners = new Map<string, (e: unknown) => void>();
  const canvas = {
    width: W, height: H,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: W, height: H }),
    setPointerCapture: () => {},
    addEventListener: (t: string, f: (e: unknown) => void) => listeners.set(t, f),
    removeEventListener: (t: string) => listeners.delete(t),
  };
  const preview = {
    activePane: "p0",
    onMap: (p: { y: number }) => p.y < 50, // the map strip, in GL coords (origin bottom-left)
    paneAt: () => "p0",
    drag: () => {}, dragMap: () => {}, wheel: () => {}, wheelMap: () => {},
    goToOnMap: () => {}, endDrag: () => {}, endDragMap: () => {},
  };
  attachSurfaceInput(canvas as unknown as HTMLCanvasElement, preview as unknown as SurfacePreview, opts);
  return { fire: (type: string, e: Record<string, unknown>) => listeners.get(type)?.(e) };
}

test("input.ts reports a pane gesture and its END, and reports the MAP strip as neither", () => {
  const seen: { pane: string; ended: boolean }[] = [];
  const h = inputHarness({ onGesture: (g) => seen.push(g) });
  // A drag on a pane: moves, then the release.
  h.fire("pointerdown", { button: 0, clientX: 400, clientY: 300, pointerId: 1 });
  h.fire("pointermove", { buttons: 1, clientX: 420, clientY: 300 });
  h.fire("pointermove", { buttons: 1, clientX: 440, clientY: 300 });
  h.fire("pointerup", { clientX: 440, clientY: 300, pointerId: 1 });
  assert.deepEqual(seen, [
    { pane: "p0", ended: false }, { pane: "p0", ended: false }, { pane: "p0", ended: true },
  ], "a drag reports its moves and exactly one end");

  // A wheel has no release, so it is never reported as ended: stillness is the host's call.
  seen.length = 0;
  h.fire("wheel", { clientX: 400, clientY: 300, deltaY: -100, preventDefault: () => {} });
  assert.deepEqual(seen, [{ pane: "p0", ended: false }]);

  // The map strip is a navigator onto the surface, not a pane's window: nothing is reported for it.
  seen.length = 0;
  h.fire("pointerdown", { button: 0, clientX: 400, clientY: 580, pointerId: 2 });
  h.fire("pointermove", { buttons: 1, clientX: 500, clientY: 580 });
  h.fire("pointerup", { clientX: 500, clientY: 580, pointerId: 2 });
  h.fire("wheel", { clientX: 400, clientY: 580, deltaY: -100, preventDefault: () => {} });
  assert.deepEqual(seen, [], "dragging the map strip says nothing about where the radio should look");
});

test("a REGION stroke is not a gesture the mode acts on: the view never moved", () => {
  const seen: { pane: string; ended: boolean }[] = [];
  const h = inputHarness({ onGesture: (g) => seen.push(g), onRegion: () => {} });
  h.fire("pointerdown", { button: 0, clientX: 400, clientY: 300, shiftKey: true, pointerId: 1 });
  h.fire("pointermove", { buttons: 1, clientX: 460, clientY: 340, shiftKey: true });
  h.fire("pointerup", { clientX: 460, clientY: 340, pointerId: 1 });
  assert.deepEqual(seen, [], "shift+drag marks a region and pans nothing, so it asks for no retune");
});

// ---------------------------------------------------------------------------
// 8. Structural guards
// ---------------------------------------------------------------------------

test("the mode names no device route of its own, and re-derives no RF arithmetic", () => {
  const src = readFileSync("src/surface/retune-mode.ts", "utf8");
  for (const route of ["/api/control/center", "/api/control/rate", "/api/control/window"]) {
    assert.ok(!src.includes(route), `retune-mode.ts must not name ${route}: the one path is applyDeviceAction`);
  }
  assert.ok(/import \{[^}]*applyDeviceAction/s.test(src), "the device is reached through T-343's one gate");
  assert.ok(/import \{[^}]*retunePlan/s.test(src), "the capture configuration must come from retunePlan");
  assert.ok(/import \{[^}]*fitToLiveWindow/s.test(src), "the narrowing must come from navigation.ts");
  const code = src.split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  assert.ok(!/28\.61|30e6|2 \*\* 20|2_000_000|20e6/.test(code), "an RF constant appeared in retune-mode.ts");
  assert.ok(!/(dcOffset|snapCenter|smallestCoveringSpan)\s*[=(]/.test(code),
    "the planner's own arithmetic was restated here");
});

test("the settle interval is the user's ~150 ms, and it is one number", () => {
  assert.equal(RETUNE_SETTLE_MS, 150);
  const src = readFileSync("src/surface/retune-mode.ts", "utf8");
  const code = src.split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  assert.equal((code.match(/\b150\b/g) ?? []).length, 1, "a second settle constant would be a second answer");
});

test("the surface host WIRES the mode: the gesture hook, both key edges, the chip and the status line", () => {
  // The mount itself is not testable headless (ADR-0013 §6: canvas/WebGL/audio never are), so the
  // wiring is asserted against its source — the same guard `surface-cutover.test.ts` uses for
  // `acceptPaneRetune(`. Each of these is a way the mode could be silently dead or silently on.
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /onGesture: \(\{ pane, ended \}\) =>/, "the gesture hook is not wired to the controller");
  assert.match(src, /retuneMode\.settled\(pane\)/, "a gesture's END does not settle the mode");
  assert.match(src, /retuneMode\.moved\(pane\)/, "a gesture's MOVES do not re-arm the settle");
  assert.match(src, /addEventListener\("keydown", onRetuneKeyDown\)/, "`R` down is not wired");
  assert.match(src, /addEventListener\("keyup", onRetuneKeyUp\)/, "`R` up is not wired: the mode could never be released");
  assert.match(src, /chromeStatus: retuneStatusFor/, "the pane's status line is not wired");
  assert.match(src, /setRetuneMode: \(on\) => retuneMode\.setSticky\(on\)/, "the chip cannot reach the mode");
  assert.match(src, /commitRetuneMode\(/, "the commit does not go through the mode's one gated path");
  // And the mode is never turned on by the mount: off by default is a property of the code, not of
  // a test's setup.
  assert.ok(!/retuneMode\.setSticky\(true\)|sticky\s*=\s*true/.test(src), "the mount turns retune mode ON");
});
