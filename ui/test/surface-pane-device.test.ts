// T-1006 — a pane's front end, visible and choosable, and the retune that names it.
//
// docs/16 §8 has always said a pane carries a `device` and what it decides: *"a pane's `device` only
// chooses whose coverage decides its grey"*. MSDR made that real in the backend (T-510 N capture
// sets, T-511 the `device_id` selector on every device route, T-512 repeatable `--device`, T-514 the
// RTL-SDR) and the UI had **no** reader for it: no pill, no picker, and a retune that posted no
// selector at all — which on a run holding two radios is `400 device_required`, because the server
// refuses to move whichever was composed first (docs/api.md, "Which radio: the device selector").
//
// Five claims, each with the control that stops a degenerate implementation passing it:
//
//  1. **The pill states whose coverage a pane draws** — and with one radio attached it names THAT
//     radio rather than the word "any", because the union of one front end is that front end.
//     Control: with two attached, the same pane on `any` must NOT name a radio.
//  2. **A retune from a pane names that pane's `device_id`**, as the body field the route reads.
//     Control: the same press from a pane on `any` with two radios attached reaches **no route** at
//     all and says why, rather than posting a request the server must refuse.
//  3. **Two panes on two devices retune two different radios** — the acceptance's spy assertion, at
//     the unit level: the two calls' `device_id`s are the two panes' devices.
//  4. **A front end this run does not hold is said, not reset.** A pane pinned to a vanished radio
//     keeps its pin (its grey is that radio's grey) and its retune is stated-and-disabled.
//  5. **"One viewport per front end" is offered only when there is more than one**, and is refused
//     out loud rather than hidden with one radio (the `RowAction` rule).
//
// Plus the guard T-407 left behind, one field wider: a second radio appearing between the label
// being painted and the button being pressed makes the press refuse with `"moved"`, because the
// same `any` no longer resolves to the radio the label named.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { FrequencyGrid } from "../src/navigation";
import type { ActiveWindow } from "../src/navigators";
import type { Lattice } from "../src/surface/lattice";
import { readoutOf } from "../src/surface/chrome";
import { PaneModel, type PaneState, type PaneStatus } from "../src/surface/panes";
import {
  ANY_DEVICE, devicePill, deviceLabel, deviceLabels, deviceRows, driverLabel, rateLabel,
  retuneDevice, splitPerDeviceOffer, shortDeviceId, type AttachedDevice,
} from "../src/surface/panedevice";
import {
  acceptPaneRetune, offerAcceptable, offerLabel, paneRetuneAction, paneRetuneOffer, paneWidthAction,
  paneWidthOffer, type PaneRetuneOffer, type PaneRetuneSite,
} from "../src/surface/retune";
import {
  commitRetuneMode, retuneModeAcceptable, retuneModeAction, retuneModeLabel, retuneModeTarget,
} from "../src/surface/retune-mode";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 120 * S, t1Ns: T0 };
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const STEP = 30e6 / 2 ** 20;

const A = "hackrf:0000000000000000aaaa0000000000ab";
const B = "rtlsdr:00000001";

const GRID: FrequencyGrid = {
  device_id: A, driver: "hackrf-one", controllable: true,
  ranges_hz: [[1e6, 6e9]], center_step: "uniform", center_step_hz: STEP,
  spans_hz: { min: 2e6, max: 20e6 }, max_live_span_hz: 20e6,
  current: { center_hz: 100.8e6, span_hz: 2.4e6 },
};

/** Two front ends, each tuned over its own band — T-513's shape, as the client reads it. */
const WINDOWS: ActiveWindow[] = [
  { deviceId: A, driver: "hackrf-one", centerHz: 100.8e6, spanHz: 2.4e6, loHz: 99.6e6, hiHz: 102e6 },
  { deviceId: B, driver: "rtl-sdr", centerHz: 433.9e6, spanHz: 2.4e6, loHz: 432.7e6, hiHz: 435.1e6 },
];

const DEV_A: AttachedDevice = { id: A, driver: "hackrf-one", centerHz: 100.8e6, sampleRateHz: 2.4e6 };
const DEV_B: AttachedDevice = { id: B, driver: "rtl-sdr", centerHz: 433.9e6, sampleRateHz: 2.4e6 };

const model = () => new PaneModel({
  bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
  freq: { centerHz: 100.8e6, spanHz: 1e6 }, spanNs: 20 * S,
});

const offerFor = (p: PaneState, attached: readonly AttachedDevice[], edgeNs = T0) =>
  paneRetuneOffer(p, WINDOWS, GRID, edgeNs, 0, attached);

/** A context whose client records every call, so "reached the device" is observable (T-343). */
function deviceSpyCtx() {
  const calls: { method: string; path: string; body: Record<string, unknown> }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: { ...s.device, loaded: true, live: true, deviceId: null, devices: [DEV_A, DEV_B] },
  }));
  const client = {
    post: (path: string, body: unknown) => {
      calls.push({ method: "POST", path, body: (body ?? {}) as Record<string, unknown> });
      return Promise.resolve({});
    },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

function siteOver(m: PaneModel, attached: readonly AttachedDevice[]): PaneRetuneSite {
  return {
    offerNow: (id: string) => { const p = m.get(id); return p ? offerFor(p, attached) : null; },
    invalidateEdge: () => 0,
  };
}

// ---------------------------------------------------------------------------
// 1. the pill: whose coverage decides this pane's grey
// ---------------------------------------------------------------------------

test("the pill names the ONE attached front end even on `any`, because the union of one radio is that radio", () => {
  const p = devicePill([DEV_A], ANY_DEVICE);
  assert.equal(p.device, ANY_DEVICE, "the pane's selector is reported as it is, not rewritten");
  assert.equal(p.label, "HackRF · 2.4 Msps", "the pill must NAME the radio, not say 'any'");
  assert.match(p.why, /One front end is attached/);
  assert.equal(p.stale, false);
  // The control: with two attached, the same pane must not name either of them.
  const both = devicePill([DEV_A, DEV_B], ANY_DEVICE);
  assert.equal(both.label, "Any of 2");
  assert.ok(!both.label.includes("HackRF"), "a union must not be labelled with one of its members");
  assert.match(both.why, /pick a front end in the viewport menu before retuning/);
});

test("a pane pinned to a front end names it and says a retune here moves THAT radio", () => {
  const p = devicePill([DEV_A, DEV_B], B);
  assert.equal(p.device, B);
  assert.equal(p.label, "RTL-SDR · 2.4 Msps");
  assert.match(p.why, new RegExp(`${B}'s alone`));
  assert.equal(p.stale, false);
});

test("a pane pinned to a front end this run does NOT hold is said, never silently reset to `any`", () => {
  const p = devicePill([DEV_A], B);
  assert.equal(p.device, B, "the pin is kept: the pane's grey is that radio's grey");
  assert.equal(p.stale, true);
  assert.match(p.label, /not attached/);
  assert.match(p.why, /this run does not hold/);
});

test("labels: a driver with no word for it is named by the backend's own word; collisions break on the id", () => {
  assert.equal(driverLabel("hackrf-one"), "HackRF");
  assert.equal(driverLabel("rtl-sdr"), "RTL-SDR");
  assert.equal(driverLabel("airspy-mini"), "airspy-mini", "an unknown driver is never renamed or placeheld");
  assert.equal(rateLabel(2.4e6), "2.4 Msps");
  assert.equal(rateLabel(20e6), "20.0 Msps");
  assert.equal(rateLabel(null), null, "nothing said, nothing said");
  assert.equal(deviceLabel({ id: B, driver: "rtl-sdr", centerHz: null, sampleRateHz: null }), "RTL-SDR");
  // Two front ends that would read identically are disambiguated by the one unique fact.
  const twin: AttachedDevice = { id: "mock:synthetic:hkpy", driver: "mock-sdr", centerHz: 1e8, sampleRateHz: 2.4e6 };
  const labels = deviceLabels([{ ...DEV_A, driver: "mock-sdr" }, twin]);
  assert.notEqual(labels.get(A), labels.get(twin.id), "two front ends must never read as the same one");
  assert.match(labels.get(twin.id)!, /hkpy/);
  assert.equal(shortDeviceId(A), "hackrf:…00ab");
});

test("the pill is on every pane's status row and NOT on the map, and `chrome.ts` stays ignorant of device_ids", () => {
  const st = (id: string, device: string): PaneStatus => ({
    id, rect: null, following: true, device, levelF: 0, levelT: 0, cellHz: 6250, cellS: 1,
    levelLabel: "6.25 kHz × 1.0 s cells (detail tier, level 0/0)", tier: "detail",
    tierLabel: "detail tier", timeLabel: "LIVE", t0Ns: 0, t1Ns: 1e9,
    freqLabel: "100.800 MHz ± 500 kHz",
    tiles: 1, fallbacks: 0, pending: 0, differsFrom: [], behind: 0, surveyed: 0, blank: 0,
    shortNs: 0, shadowLabel: null,
  } as unknown as PaneStatus);
  // `deviceFor` is the LAST slot (T-1028's `statusFor` is the one before it): the row is a list of
  // independent, anonymous slots, and a new one goes on the end rather than in the middle.
  const r = readoutOf([st("pane1", A), st("pane2", B), st("map", ANY_DEVICE)], "map", null, null, null, null,
    (id) => devicePill([DEV_A, DEV_B], id === "pane1" ? A : id === "pane2" ? B : ANY_DEVICE));
  assert.equal(r.rows[0].device?.label, "HackRF · 2.4 Msps");
  assert.equal(r.rows[1].device?.label, "RTL-SDR · 2.4 Msps");
  assert.equal(r.rows[2].device, null, "the map is not a window you look through at one radio");
  // The headline states the WINDOW; the device is the pill's job (it used to be a bare id appended
  // to the headline with no word for what it meant).
  assert.ok(!r.rows[0].headline.includes(A), `the headline restated the device_id: ${r.rows[0].headline}`);
  // Structural: the chrome must not learn what a device is, for the same import-graph reason it must
  // not learn what a span is (`surface-retune.test.ts` greps it for "spanHz").
  // Comments explaining WHY the slot is anonymous are the point, not a dependency — stripped the
  // same way `surface-retune.test.ts` strips them for its own version of this check.
  const src = readFileSync("src/surface/chrome.ts", "utf8")
    .split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  for (const word of ["panedevice", "device_id", "driver", "Msps", "hackrf"]) {
    assert.ok(!src.includes(word),
      `chrome.ts names "${word}": the device slot is strings and a bit, or the preview page reaches a radio`);
  }
});

// ---------------------------------------------------------------------------
// 2–3. the retune names the pane's device
// ---------------------------------------------------------------------------

test("THE PROPERTY: a retune from a pane names that pane's device_id in the request body", async () => {
  const m = model();
  const id = m.list()[0].id;
  m.setDevice(id, B);
  m.setFreq(id, 433.9e6, 1e6);
  const o = offerFor(m.get(id)!, [DEV_A, DEV_B]);
  assert.equal(o.retuneDeviceId, B, "the offer must name the front end it planned against");
  assert.equal(offerAcceptable(o), true, offerLabel(o));
  const { ctx, calls } = deviceSpyCtx();
  const r = await acceptPaneRetune(ctx, siteOver(m, [DEV_A, DEV_B]), o);
  assert.equal(r.ok, true, `the press was refused: ${JSON.stringify(r)}`);
  assert.equal(calls.length, 1, "one user retune is one request (T-529)");
  assert.equal(calls[0].path, "/api/control/window");
  assert.equal(calls[0].body.device_id, B, "the request did not name the pane's radio");
});

test("THE ACCEPTANCE, at the unit level: two panes on two devices retune two DIFFERENT radios", async () => {
  const m = model();
  const p1 = m.list()[0].id;
  const p2 = m.split(p1, "columns")!;
  m.setDevice(p1, A);
  m.setDevice(p2, B);
  m.setFreq(p1, 100.8e6, 1e6);
  m.setFreq(p2, 433.9e6, 1e6);
  const attached = [DEV_A, DEV_B];
  const { ctx, calls } = deviceSpyCtx();
  for (const id of [p1, p2]) {
    const o = offerFor(m.get(id)!, attached);
    const r = await acceptPaneRetune(ctx, siteOver(m, attached), o);
    assert.equal(r.ok, true, `${id}: ${JSON.stringify(r)}`);
  }
  assert.deepEqual(calls.map((c) => c.body.device_id), [A, B],
    "each retune must name its OWN pane's front end");
});

test("the CONTROL: a pane on `any` with two radios attached reaches NO route, and says why", async () => {
  const m = model();
  const id = m.list()[0].id;
  assert.equal(m.get(id)!.device, ANY_DEVICE, "the default is the union");
  const o = offerFor(m.get(id)!, [DEV_A, DEV_B]);
  assert.equal(o.block, "device_required");
  assert.equal(o.retuneDeviceId, null, "nothing may be invented where nothing was said");
  assert.equal(offerAcceptable(o), false);
  assert.match(offerLabel(o), /pick a front end for it in the viewport menu/);
  assert.equal(paneRetuneAction(o), null, "an unresolvable pane must not become a device action");
  const { ctx, calls } = deviceSpyCtx();
  const r = await acceptPaneRetune(ctx, siteOver(m, [DEV_A, DEV_B]), o);
  assert.deepEqual(r, { ok: false, reason: "not_acceptable" });
  assert.deepEqual(calls, [], "a request the server must refuse was sent anyway");
});

test("a pane pinned to a vanished front end is stated and disabled, and its pin is kept", () => {
  const m = model();
  const id = m.list()[0].id;
  m.setDevice(id, B);
  const o = offerFor(m.get(id)!, [DEV_A]); // B unplugged
  assert.equal(o.device, B, "the pane still names the radio it was pinned to");
  assert.equal(o.block, "device_gone");
  assert.equal(offerAcceptable(o), false);
  assert.match(offerLabel(o), /does not hold/);
});

test("one radio, or none: the request is byte-identical to before the selector existed", () => {
  const m = model();
  const id = m.list()[0].id;
  // One attached: `any` resolves to it, and the action names it (which the route accepts as "the"
  // device on a single-SDR run).
  const one = offerFor(m.get(id)!, [DEV_A]);
  assert.equal(one.retuneDeviceId, A);
  assert.equal(paneRetuneAction(one)!.deviceId, A);
  // None attached (a replay, or a caller that never passed a list at all): no selector, and the
  // route's own `not_live` answers rather than an invented `unknown_device`.
  const none = paneRetuneOffer(m.get(id)!, WINDOWS, GRID, T0);
  assert.equal(none.retuneDeviceId, null);
  assert.equal(none.block, null, "no front end reported is not a device refusal");
  assert.equal(paneRetuneAction(none)!.deviceId, null);
});

test("the width presets reach the same routes, so they name the pane's device too", () => {
  const m = model();
  const id = m.list()[0].id;
  m.setDevice(id, B);
  const w = paneWidthOffer(m.get(id)!, 2e6, GRID, T0, 0, [DEV_A, DEV_B]);
  assert.equal(paneWidthAction(w)!.deviceId, B);
  const ambiguous = paneWidthOffer(model().list()[0], 2e6, GRID, T0, 0, [DEV_A, DEV_B]);
  assert.equal(ambiguous.block, "device_required");
  assert.equal(paneWidthAction(ambiguous), null);
});

test("T-407's guard, one field wider: a second radio appearing between paint and press refuses", async () => {
  const m = model();
  const id = m.list()[0].id;
  // Painted while ONE radio was attached, so `any` resolved to it and the label named it.
  const painted = offerFor(m.get(id)!, [DEV_A]);
  assert.equal(painted.retuneDeviceId, A);
  const { ctx, calls } = deviceSpyCtx();
  // …and a second front end is composed before the press. The same `any` now means the union.
  const r = await acceptPaneRetune(ctx, siteOver(m, [DEV_A, DEV_B]), painted);
  assert.deepEqual(r, { ok: false, reason: "moved" });
  assert.deepEqual(calls, [], "the press moved a radio the label had named by a rule that no longer held");
});

// ---------------------------------------------------------------------------
// 4–5. the picker and the split offer
// ---------------------------------------------------------------------------

test("the picker offers the union plus every attached front end, marking the pane's own choice", () => {
  const rows = deviceRows([DEV_A, DEV_B], B);
  assert.deepEqual(rows.map((r) => r.id), [ANY_DEVICE, A, B], "the union first, then composition order");
  assert.deepEqual(rows.map((r) => r.on), [false, false, true]);
  assert.match(rows[0].hint, /a retune must name one/, "the union's cost is stated in the picker itself");
  // With one radio the union's row names it, so the two rows are not two identical-looking choices.
  assert.match(deviceRows([DEV_A], ANY_DEVICE)[0].label, /HackRF/);
  // A stale pin is offered as a row too, or the pane could not show what it is pinned to.
  const stale = deviceRows([DEV_A], B);
  assert.equal(stale.at(-1)!.id, B);
  assert.equal(stale.at(-1)!.on, true);
});

test("`one viewport per front end` is offered with two radios and REFUSED OUT LOUD with one", () => {
  const two = splitPerDeviceOffer([DEV_A, DEV_B]);
  assert.equal(two.enabled, true);
  assert.deepEqual(two.devices, [A, B]);
  assert.match(two.why, /2 viewports, one pinned to each front end/);
  assert.match(two.why, /no radio moves/, "the offer must say it is a view change");
  const one = splitPerDeviceOffer([DEV_A]);
  assert.equal(one.enabled, false);
  assert.deepEqual(one.devices, []);
  assert.match(one.why, /Only HackRF · 2\.4 Msps is attached/);
  assert.equal(splitPerDeviceOffer([]).enabled, false);
});

test("retuneDevice: every case the backend distinguishes, and no other answer", () => {
  assert.deepEqual(retuneDevice([DEV_A, DEV_B], A), { kind: "named", deviceId: A });
  assert.deepEqual(retuneDevice([DEV_A], ANY_DEVICE), { kind: "named", deviceId: A });
  assert.deepEqual(retuneDevice([], ANY_DEVICE), { kind: "default" });
  assert.deepEqual(retuneDevice([DEV_A, DEV_B], ANY_DEVICE), { kind: "ambiguous", ids: [A, B] });
  assert.deepEqual(retuneDevice([DEV_A], B), { kind: "gone", requested: B, ids: [A] });
});

// ---------------------------------------------------------------------------
// T-1028's retune mode reaches the front end too, so it names the pane's radio
// ---------------------------------------------------------------------------

test("retune mode — the one path a GESTURE opens — names the pane's device, and refuses out loud when it cannot", async () => {
  const m = model();
  const id = m.list()[0].id;
  m.setDevice(id, B);
  m.setFreq(id, 433.9e6, 1e6);
  const target = retuneModeTarget(m.get(id)!, GRID, T0, 0, [DEV_A, DEV_B]);
  assert.equal(target.retuneDeviceId, B);
  assert.equal(target.deviceBlock, null);
  assert.equal(retuneModeAcceptable(target), true, retuneModeLabel(target));
  assert.equal(retuneModeAction(target).deviceId, B, "a settled view must tune ITS OWN pane's radio");
  const { ctx, calls } = deviceSpyCtx();
  const site = { targetNow: () => retuneModeTarget(m.get(id), GRID, T0, 0, [DEV_A, DEV_B]), invalidateEdge: () => 0 };
  const r = await commitRetuneMode(ctx, site, id);
  assert.equal(r.ok, true, `the commit was refused: ${JSON.stringify(r)}`);
  assert.equal(calls[0].body.device_id, B);

  // The CONTROL: the same settled view from a pane on the union of two radios sends nothing at all —
  // the mode is a gesture path, so it is exactly the one that would otherwise post the request the
  // server answers `400 device_required`.
  const union = retuneModeTarget(model().list()[0], GRID, T0, 0, [DEV_A, DEV_B]);
  assert.equal(union.deviceBlock, "device_required");
  assert.equal(union.retuneDeviceId, null);
  assert.equal(retuneModeAcceptable(union), false);
  assert.equal(retuneModeAction(union), null);
  assert.match(retuneModeLabel(union), /pick a front end for it in the viewport menu/);
  const spy2 = deviceSpyCtx();
  const out = await commitRetuneMode(spy2.ctx, { targetNow: () => union, invalidateEdge: () => 0 }, union.paneId);
  assert.equal(out.ok, false);
  assert.deepEqual(spy2.calls, [], "retune mode posted a request the server must refuse");
});
