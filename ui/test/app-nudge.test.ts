// T-409: tuning nudge buttons — shift the tuned centre off the DC/LO spike without losing the
// signal from the band.
//
// The pure arithmetic (snap, refusal, landing point, reversibility) is asserted in
// `navigation.test.ts` against `nudgePlan`. This file asserts the CONTROL: what the six buttons
// say, which of them are disabled and why, and — the part that matters most for a `core_interface`
// task — that a press reaches the front end through T-343's single gate and nowhere else, leaving
// T-340's "no pan and no wheel reaches a device route" exactly as it was.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { NUDGE_FRACTIONS, NUDGE_IDEAL_FRACTION, type CenterGrid } from "../src/navigation";
import { NUDGE_HINT, nudgeButtons, type NudgeInput } from "../src/app/centre/nudge";
import { applyDeviceAction, retuneAction, viewHooks, NOT_LIVE_TEXT } from "../src/app/centre/view";
import * as ax from "../src/axis";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";

const STEP = 30e6 / 2 ** 20;
const GRID: CenterGrid = { ranges_hz: [[1e6, 6e9]], center_step_hz: STEP };
const DEV = "hackrf:0000000000000000fake0000000000ab";

const input = (over: Partial<NudgeInput> = {}): NudgeInput => ({
  live: true, fromHz: 100.8e6, spanHz: 2.4e6, grid: GRID, deviceId: DEV, busy: false, ...over,
});

/** A context whose client records every call, so "reached the device" is observable — the same
 * construction `app-centre.test.ts` uses for T-343. */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: { ...s.device, loaded: true, live, deviceId: DEV, centerHz: 100.8e6, sampleRateHz: 2.4e6, centerGrid: GRID },
    live: { ...s.live, centerHz: 100.8e6, bandwidthHz: 2.4e6, bins: 1024, view: { loHz: 100.3e6, hiHz: 101.3e6 } },
  }));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

test("T-409: six buttons, one per direction per offered fraction, reading along the frequency axis", () => {
  const bs = nudgeButtons(input());
  assert.equal(bs.length, 2 * NUDGE_FRACTIONS.length);
  // Widest leftward at the left end, widest rightward at the right — the row reads like the axis
  // it moves.
  assert.deepEqual(bs.map((b) => b.fraction), [-1 / 2, -1 / 4, -1 / 8, 1 / 8, 1 / 4, 1 / 2]);
  assert.deepEqual(bs.map((b) => b.label), ["◂½", "◂¼", "◂⅛", "⅛▸", "¼▸", "½▸"]);
  // Every button says which way and by how much, for a screen reader that never sees the arrow.
  assert.equal(bs[0].ariaLabel, "Nudge the tuned centre down by a half of the span");
  assert.equal(bs[4].ariaLabel, "Nudge the tuned centre up by a quarter of the span");
  // And the group says what the whole thing is FOR, which six arrows cannot.
  assert.match(NUDGE_HINT, /DC\/LO spike/);
});

test("T-409 THE HAZARD: ½ span is offered, marked, and explained — never silently the biggest dodge", () => {
  const bs = nudgeButtons(input());
  const half = bs.find((b) => b.fraction === 1 / 2)!;
  const quarter = bs.find((b) => b.fraction === NUDGE_IDEAL_FRACTION)!;
  const eighth = bs.find((b) => b.fraction === 1 / 8)!;

  // The user asked for ½ and gets it: enabled and pressable, because it is also the right stride
  // for stepping along a band.
  assert.equal(half.disabled, false);
  assert.ok(half.centerHz !== null);
  // But it is the one that trades the DC spike for the anti-alias skirt, and it says so, naming
  // the alternative rather than just warning.
  assert.equal(half.hazard, "edge");
  assert.match(half.title, /band edge/);
  assert.match(half.title, /anti-alias/);
  assert.match(half.title, /¼ span/);
  assert.equal(half.ideal, false);

  // ¼ is the derived ideal — the midpoint of the usable half-band — and is marked as such.
  assert.equal(quarter.ideal, true);
  assert.equal(quarter.hazard, null);
  assert.match(quarter.title, /derived ideal/);
  assert.match(quarter.title, /maximally far/);

  // ⅛ is neither: a real dodge, no edge problem, no claim to be the best one.
  assert.equal(eighth.ideal, false);
  assert.equal(eighth.hazard, null);
  assert.ok(!/derived ideal/.test(eighth.title));

  // Each title states what the press actually does — where a centred signal lands, and how much of
  // the window survives — in numbers taken from the plan, not a generic caution.
  assert.match(eighth.title, /lands 25% of the way to the window edge/);
  assert.match(eighth.title, /88% of the window is kept/);
  assert.match(quarter.title, /lands 50% of the way to the window edge/);
  assert.match(half.title, /lands 100% of the way to the window edge/);
  assert.match(half.title, /50% of the window is kept/);
});

test("T-409 THE CONTROL: a nudge that cannot be taken in full is DISABLED, and says it is not clamped", () => {
  // A centre where every upward nudge runs off the top of the band (the narrowest is ⅛ of 2.4 MHz
  // = 300 kHz), and no downward one does.
  const bs = nudgeButtons(input({ fromHz: 6e9 - 0.2e6 }));
  const up = bs.filter((b) => b.fraction > 0);
  const down = bs.filter((b) => b.fraction < 0);
  assert.deepEqual(up.map((b) => b.disabled), [true, true, true]);
  assert.deepEqual(up.map((b) => b.centerHz), [null, null, null], "a refused press carries no centre to post");
  assert.deepEqual(down.map((b) => b.disabled), [false, false, false], "refusing is per-button, not a dead row");

  // And the reason is the one the ticket asked for out loud: the alternative is a control that
  // moves less than its own label.
  assert.match(up[0].title, /Cannot be taken in full/);
  assert.match(up[0].title, /outside what hackrf:.* can tune/);
  assert.match(up[0].title, /disabled rather than clamped/);
  assert.match(up[0].title, /would move less than the button says/);

  // The other refusals disable too, each saying which fact is missing rather than "unavailable".
  assert.match(nudgeButtons(input({ grid: null }))[0].title, /no tunable range/);
  assert.match(nudgeButtons(input({ fromHz: null }))[0].title, /tuned centre is not known/);
  assert.match(nudgeButtons(input({ spanHz: null }))[0].title, /tuned span is not known/);
  for (const bad of [{ grid: null }, { fromHz: null }, { spanHz: null }]) {
    assert.ok(nudgeButtons(input(bad)).every((b) => b.disabled && b.centerHz === null));
  }

  // A replay has no radio to move, and says the same thing every other retune path says.
  const replay = nudgeButtons(input({ live: false }));
  assert.ok(replay.every((b) => b.disabled && b.centerHz === null));
  assert.equal(replay[0].title, NOT_LIVE_TEXT);
});

test("T-409 THE SNAP: the actual move is named, and the difference from the advertised fraction is not absorbed", () => {
  const b = nudgeButtons(input())[4]; // +1/4
  assert.ok(b.centerHz !== null);
  // The centre on the button is on the synthesiser grid…
  assert.equal(b.centerHz!, Math.round(b.centerHz! / STEP) * STEP);
  // …so the shift is not exactly span/4, and the tooltip says so rather than printing the fraction
  // as if it were the distance.
  assert.notEqual(b.centerHz! - 100.8e6, 2.4e6 / 4);
  assert.match(b.title, /Snapped to the tuning grid/);
  assert.match(b.title, /the advertised fraction/);
  // It names the radio it will move and what a retune costs (T-343's rule for a device action).
  assert.match(b.title, /Moves the radio on hackrf:/);
  assert.match(b.title, /briefly interrupts capture/);

  // A source with no stated tuning step is tuned anyway, but nothing claims it sits exactly there.
  const u = nudgeButtons(input({ grid: { ranges_hz: [[1e6, 6e9]], center_step_hz: null } }))[4];
  assert.equal(u.disabled, false);
  assert.match(u.title, /states no tuning step/);
  assert.ok(!/Snapped to the tuning grid/.test(u.title));
});

test("T-409: while a nudge is in flight the row is disabled — one capture at a time, one settle gap", () => {
  const busy = nudgeButtons(input({ busy: true }));
  assert.ok(busy.every((b) => b.disabled), "a second press must not race the first to the device");
  // Busy is not a refusal: the centre is still computed, so the row comes back rather than
  // re-deriving itself from scratch.
  assert.ok(busy.filter((b) => b.centerHz !== null).length === busy.length);
});

test("T-409 THE PROPERTY: a press reaches the front end exactly once, through the one gate", async () => {
  const { ctx, calls } = deviceSpyCtx();
  const b = nudgeButtons(input())[4]; // +1/4
  // This is precisely what the mount does on click — a typed `DeviceAction`, not a route.
  await applyDeviceAction(ctx, retuneAction(b.centerHz!, "nudge"));
  // The centre goes out exactly as planned: re-snapping an already-snapped centre is a no-op, so
  // the number the tooltip showed is the number the radio is asked for.
  assert.deepEqual(calls, [{ method: "POST", path: "/api/control/center", body: { center_hz: b.centerHz } }]);
  // No rate call: a nudge moves the centre and nothing else, so the capture is not re-plumbed to a
  // different sample rate on top of the retune.
  assert.deepEqual(calls.filter((c) => c.path === "/api/control/rate"), []);
  assert.match(ctx.store.get().toast.text, /Retuning hackrf:/);
});

test("T-409 THE CONTROL THAT MUST STILL HOLD: no pan and no wheel reaches a device route (T-340)", () => {
  const { ctx, calls } = deviceSpyCtx();
  const hooks = viewHooks(ctx);
  const g = { centerHz: 100.8e6, bandwidthHz: 2.4e6, bins: 1024 };
  const v = ctx.store.get().live.view!;
  const w = v.hiHz - v.loHz;
  // A ±1.0 drag of the whole bar, and then some — twenty times the old overflow threshold.
  for (const overflowHz of [0.06 * w, w, 10 * w, 6e9, -0.06 * w, -w, -10 * w, -6e9]) {
    const p = ax.panView(g, v, overflowHz);
    hooks.setView(p.view);
    hooks.edgeOffer(ax.panRetuneCenter(p.view, p.overflowHz), { loHz: p.view.loHz + p.overflowHz, hiHz: p.view.hiHz + p.overflowHz });
  }
  assert.deepEqual(calls, [], "adding an explicit button must not have opened a gesture path to the radio");
  assert.ok(ctx.store.get().live.retuneOffer, "a pan still only offers");
});

test("T-409: the nudge names no device route — the gate stays a type, not a convention", () => {
  const src = readFileSync("src/app/centre/nudge.ts", "utf8");
  for (const r of ["/api/control/center", "/api/control/rate", "/api/control/window", "/api/control/gains", "/api/control/bias_tee", "/api/control/baseband_filter"]) {
    assert.ok(!src.includes(r), `nudge.ts must not name ${r} — it goes through view.ts's DeviceAction`);
  }
  assert.ok(src.includes("applyDeviceAction"), "…and it must actually go through that gate");
  // The fractions the user asked for, and no fourth one smuggled in beside them.
  assert.deepEqual([...NUDGE_FRACTIONS], [1 / 8, 1 / 4, 1 / 2]);
  // It is mounted: a control nobody can see is not a control.
  assert.match(readFileSync("src/app/centre/index.ts", "utf8"), /nudge: mountNudge/);
  assert.match(readFileSync("src/app/index.html", "utf8"), /data-slot="nudge"/);
});
