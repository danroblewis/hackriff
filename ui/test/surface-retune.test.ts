// T-444 — retune to the viewport. Taking the offer is the one act that moves the radio, and it goes
// through T-343's single gate.
//
// **T-476 corrected the trigger, on the user's own argument.** T-444 produced an offer only for a
// viewport *not contained* in a tuned window, on the premise that a retune is for reaching spectrum
// you cannot see. Retuning to a viewport **inside** the window raises the resolution there, so the
// control is now persistent and per-pane, and the three conditions that used to erase it are stated
// and disabled instead. The tests for that are in their own section below; the pair that matter most
// are **the contained viewport is still retunable** (the ticket) and **a pane frozen between the
// paint and the press refuses** (the hole a persistent control opens in T-407's guard).
//
// Four claims, each with the control that stops a degenerate implementation passing it:
//
//  1. **A pan alone never commands the radio.** T-340's control, restated over the pane vocabulary:
//     drag ±1.0 of the whole 6 GHz surface, wheel-zoom at every scale, and compute the offer after
//     every single step — the call list must be **empty**. Control: the run must actually *produce*
//     an offer, or "it never offers anything" would pass.
//  2. **The explicit act does**, through `applyDeviceAction` — the rate first only when the window
//     in force is the wrong width, then the centre, snapped to the achievable grid, with the
//     backend's `device_id` in the toast rather than a client-invented one.
//  3. **Snapping and off-DC placement are reused, not reinvented** (T-341's `snapCenter`, T-418's
//     `span/4`). Asserted on the *plan*, so a second copy of the arithmetic would have to agree
//     with the first to pass — and asserted on the source, so it cannot be a second copy at all.
//  4. **At the band edge the offer is disabled, never clamped** (T-409's honesty rule), and a pane
//     that moved after the offer was drawn refuses with `"moved"` rather than retuning to the new
//     place (T-407's failure mode, structurally).
//
// Plus the spike's one client-side ask (T-437 §5.2): the growing edge's tiles are invalidated after
// a **successful** retune, and only then.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { retunePlan, type FrequencyGrid, type RetunePlan } from "../src/navigation";
import type { ActiveWindow } from "../src/navigators";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import { readoutOf } from "../src/surface/chrome";
import { PaneModel, type PaneState, type PaneStatus } from "../src/surface/panes";
import {
  acceptPaneRetune, acceptPaneWidth, coveringWindow, offerAcceptable, offerLabel, paneRetuneAction,
  paneRetuneOffer, paneWidthAction, paneWidthOffer, widthOfferAcceptable, widthOfferLabel,
  type PaneRetuneOffer, type PaneRetuneSite, type PaneWidthOffer, type PaneWidthSite,
} from "../src/surface/retune";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 120 * S, t1Ns: T0 };
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const STEP = 30e6 / 2 ** 20; // HACKRF_ONE_TUNING_STEP_HZ, ≈ 28.6102294921875 Hz

/** The achievable (centre, span) grid a HackRF-class front end reports on `GET /api/navigation`. */
const GRID: FrequencyGrid = {
  device_id: "hackrf:0000000000000000fake0000000000ab", driver: "hackrf-one", controllable: true,
  ranges_hz: [[1e6, 6e9]], center_step: "uniform", center_step_hz: STEP,
  spans_hz: { min: 2e6, max: 20e6 }, max_live_span_hz: 20e6,
  current: { center_hz: 100e6, span_hz: 2.4e6 },
};

/** One front end, tuned where the panes start. */
const WINDOWS: ActiveWindow[] = [{
  deviceId: "hackrf:0000000000000000fake0000000000ab", driver: "hackrf-one",
  centerHz: 100.8e6, spanHz: 2.4e6, loHz: 99.6e6, hiHz: 102e6,
}];

const model = () => new PaneModel({
  bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
  freq: { centerHz: 100.8e6, spanHz: 1e6 }, spanNs: 20 * S,
});

const offerFor = (p: PaneState, edgeNs = T0, windows = WINDOWS, grid: FrequencyGrid | null = GRID) =>
  paneRetuneOffer(p, windows, grid, edgeNs);

/** A context whose client records every call, so "reached the device" is observable (T-343). */
function deviceSpyCtx(live = true) {
  const calls: { method: string; path: string; body: unknown }[] = [];
  const store = createStore(initialState());
  store.set((s) => ({
    device: {
      ...s.device, loaded: true, live, deviceId: GRID.device_id, sampleRateHz: 2_400_000,
      centerHz: 100.8e6,
      centerGrid: { ranges_hz: GRID.ranges_hz, center_step_hz: GRID.center_step_hz },
    },
  }));
  const client = {
    post: (path: string, body: unknown) => { calls.push({ method: "POST", path, body }); return Promise.resolve({}); },
    get: (path: string) => { calls.push({ method: "GET", path, body: null }); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  return { ctx: { store, client, token: "t" } as AppContext, calls };
}

/** A site whose `offerNow` re-derives from the live model, which is what makes the guard a guard. */
function siteOver(m: PaneModel, edgeNs = T0): PaneRetuneSite & { invalidations: number } {
  const site = {
    invalidations: 0,
    offerNow: (id: string) => { const p = m.get(id); return p ? offerFor(p, edgeNs) : null; },
    invalidateEdge: () => { site.invalidations++; return 7; },
  };
  return site;
}

// ---------------------------------------------------------------------------
// 1. THE CONTROL: a pan is a pan
// ---------------------------------------------------------------------------

test("T-340's control holds for a pane: pan and wheel across the whole 6 GHz surface reach NO route", () => {
  const calls: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...args: unknown[]) => { calls.push(args); return Promise.reject(new Error("a pan must not reach the network")); };
  const offers: (PaneRetuneOffer | null)[] = [];
  try {
    const m = model();
    const a = m.list()[0].id;
    const b = m.split(a, "columns")!;
    // Drags of ±1.0 of the whole bar, in both directions, plus the small ones a finger makes — and
    // the offer is recomputed after EVERY step, because computing it is what a mount does on move.
    const full = BOUNDS.f1Hz - BOUNDS.f0Hz;
    for (const frac of [0.001, 0.05, 0.5, 1, -0.001, -0.5, -1, 0.25]) {
      m.panFreq(a, frac * full);
      offers.push(offerFor(m.get(a)!));
      m.panFreq(b, -frac * full);
      offers.push(offerFor(m.get(b)!));
    }
    for (const factor of [0.01, 0.5, 2, 100, 1000]) {
      m.zoomFreq(a, factor, 0.25);
      offers.push(offerFor(m.get(a)!));
    }
    // …and the time axis, which can never justify a retune at all.
    m.panTime(a, -60 * S); m.zoomTime(a, 4); m.pause(b); m.follow(b);
    offers.push(offerFor(m.get(a)!), offerFor(m.get(b)!));
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(calls, [], "a pane pan reached the network: a pan must never command the radio");
  // The control that stops "it never offers" from passing this.
  assert.ok(offers.some((o) => offerAcceptable(o)), "no acceptable offer was produced, so the run proves nothing");
});

// ---------------------------------------------------------------------------
// T-476: the control is PERSISTENT, and containment was the wrong trigger
// ---------------------------------------------------------------------------

test("T-476 THE PROPERTY: a viewport zoomed INSIDE the tuned window is still offered a retune, and it is narrower", () => {
  // The user's complaint, stated as the case that used to produce nothing. The pane opens at
  // 100.8 MHz ± 500 kHz, wholly inside the 99.6–102 MHz window in force, and then zooms in further.
  const m = model();
  const p = m.list()[0].id;
  m.zoomFreq(p, 0.2);                      // 200 kHz across, deep inside the 2.4 MHz capture
  const box = m.get(p)!;
  assert.ok(coveringWindow(WINDOWS, box.freq.centerHz - box.freq.spanHz / 2, box.freq.centerHz + box.freq.spanHz / 2),
    "the run needs the pane to be CONTAINED, or it is not testing the case that regressed");
  const o = offerFor(box);
  assert.equal(o.covered?.deviceId, GRID.device_id, "the covering window is reported, not used to erase the offer");
  assert.equal(offerAcceptable(o), true, "a contained viewport must still be retunable — that is the whole ticket");
  assert.ok(o.plan.ok);
  if (!o.plan.ok) return;
  // …and what it buys is real: the planned capture is narrower than the one in force, which is the
  // resolution argument the user made. 2 Msps is the HackRF's floor and the window is 2.4 MHz.
  assert.ok(o.plan.spanHz < WINDOWS[0].spanHz, `${o.plan.spanHz} is not narrower than ${WINDOWS[0].spanHz}`);
  assert.match(offerLabel(o), /narrower than the 2\.400 MHz capture in force/);
});

test("T-476: what the retune BUYS is stated, and a re-centre is not sold as a sharpening", () => {
  // A pane as wide as the window it sits in: the plan cannot be narrower, so the label says so
  // rather than implying finer cells. (Honesty, not politeness — it is the same rule as T-409's
  // disabled-not-clamped and the grey rule: never claim detail the configuration will not produce.)
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
  });
  const o = offerFor(m.list()[0]);
  assert.ok(o.covered, "the pane fills the window exactly, so it is contained");
  assert.ok(o.plan.ok && o.plan.spanHz >= WINDOWS[0].spanHz);
  assert.match(offerLabel(o), /no narrower than the 2\.400 MHz capture in force, so this re-centres rather than sharpens/);
});

test("T-476: a pane panned off the tuned window is offered a retune, as it always was", () => {
  const m = model();
  const p = m.list()[0];
  assert.equal(coveringWindow(WINDOWS, 100.3e6, 101.3e6)?.deviceId, GRID.device_id);
  m.panFreq(p.id, 300e6);
  const o = offerFor(m.get(p.id)!);
  assert.equal(o.covered, null, "panned off the window, nothing covers it");
  assert.ok(o.plan.ok, "panning to un-tuned spectrum must offer a retune");
});

test("T-476: the control has a row on EVERY pane and none on the map, and the chrome stays ignorant of tuning", () => {
  // Where the persistent control actually lives: a slot on each viewport's chrome row. Asserted on
  // `readoutOf`, which is the pure half — the DOM half is `ui/e2e/surface-retune.e2e.mjs`, because a
  // node test has no document and "a control that renders is not a control that is reachable".
  const st = (id: string): PaneStatus => ({
    id, rect: null, following: true, device: "any", levelF: 0, levelT: 0, cellHz: 6250, cellS: 1,
    levelLabel: "6.25 kHz × 1.0 s cells (detail tier, level 0/0)", tier: "detail",
    tierLabel: "detail tier: the live chain's own lattice, at the resolution the front end measured.",
    timeLabel: "LIVE", freqLabel: "100.800 MHz ± 500 kHz",
    tiles: 1, fallbacks: 0, pending: 0, differsFrom: [],
  });
  const asked: string[] = [];
  const r = readoutOf([st("pane1"), st("pane2"), st("map")], "map", (id) => {
    asked.push(id);
    return { label: "Retune", why: `why ${id}`, enabled: id === "pane1" };
  });
  assert.deepEqual(asked, ["pane1", "pane2"], "the map was asked for a control, or a pane was not");
  assert.deepEqual(r.rows.map((x) => x.action?.why ?? null), ["why pane1", "why pane2", null]);
  assert.deepEqual(r.rows.map((x) => x.action?.enabled ?? null), [true, false, null],
    "a disabled control is still ON the row: a missing control teaches the user nothing");
  // And with no host supplying one, the readout is exactly what it was before T-476.
  assert.deepEqual(readoutOf([st("pane1")], null).rows.map((x) => x.action), [null]);
  // The slot is strings and a bit. `chrome.ts` may not know what a retune is, or `retune.ts` — and
  // with it `applyDeviceAction` — lands in the /surface.html preview's import graph, which
  // `surface-preview.test.ts` forbids.
  // Comments explaining why the slot is anonymous are the point, not a dependency.
  const src = readFileSync("src/surface/chrome.ts", "utf8")
    .split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  for (const word of ["retune", "Retune", "centerHz", "spanHz", "/api/"]) {
    assert.ok(!src.includes(word), `chrome.ts names "${word}": the control's slot must stay anonymous`);
  }
});

test("T-476: there is NO pane state that produces no control — every one gets a sentence", () => {
  // "Nothing said is never permissive." The three conditions that used to erase the control are
  // enumerated here against `offerLabel`, so a future refusal that forgets to say anything fails.
  const cases: { what: string; pane: PaneState; grid: FrequencyGrid | null; re: RegExp }[] = [];
  const paneWith = (freq: { centerHz: number; spanHz: number }, back = 0) => {
    const m = new PaneModel({ bounds: BOUNDS, lattice: LAT, width: 1200, height: 800, freq, spanNs: 20 * S });
    const id = m.list()[0].id;
    if (back) m.panTime(id, -back);
    return m.get(id)!;
  };
  cases.push({ what: "contained in the tuned window", pane: paneWith({ centerHz: 100.8e6, spanHz: 200e3 }), grid: GRID, re: /^Retune to 100\./ });
  cases.push({ what: "off the tuned window", pane: paneWith({ centerHz: 433.92e6, spanHz: 200e3 }), grid: GRID, re: /^Retune to 433\./ });
  cases.push({ what: "frozen in the past", pane: paneWith({ centerHz: 433.92e6, spanHz: 200e3 }, 60 * S), grid: GRID, re: /frozen behind the growing edge/ });
  cases.push({ what: "wider than one live window", pane: paneWith({ centerHz: 3e9, spanHz: 400e6 }), grid: GRID, re: /survey overview/ });
  cases.push({ what: "no grid reported", pane: paneWith({ centerHz: 433.92e6, spanHz: 200e3 }), grid: null, re: /has not reported a tunable range/ });
  for (const c of cases) {
    const o = offerFor(c.pane, T0, WINDOWS, c.grid);
    assert.ok(o, `${c.what}: no offer at all — the control would vanish`);
    const label = offerLabel(o);
    assert.match(label, c.re, `${c.what}: ${label}`);
    assert.ok(label.length > 20, `${c.what}: the sentence says nothing`);
  }
  // The control that stops "every case is disabled" from passing this: two of the five are takeable
  // and three are stated refusals, so the enumeration is measuring a difference.
  const takeable = cases.filter((c) => offerAcceptable(offerFor(c.pane, T0, WINDOWS, c.grid)));
  assert.equal(takeable.length, 2, `expected exactly the two live, in-range panes to be takeable: ${takeable.map((c) => c.what)}`);
});

test("a pane straddling the tuned window's edge is un-tuned: coverage is containment, not overlap", () => {
  assert.equal(coveringWindow(WINDOWS, 101.5e6, 102.5e6), null);
  // And a pane pinned to another front end is not covered by this one's window.
  assert.equal(coveringWindow(WINDOWS, 100.3e6, 101.3e6, "hackrf:other"), null);
  assert.ok(coveringWindow(WINDOWS, 100.3e6, 101.3e6, GRID.device_id!));
});

test("a pane frozen in the past is STATED and disabled: a retune cannot change what was already captured", () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  assert.equal(offerAcceptable(offerFor(m.get(p)!)), true, "the live pane is offered a retune");
  m.panTime(p, -60 * S);
  const o = offerFor(m.get(p)!);
  assert.equal(o.block, "past");
  assert.equal(offerAcceptable(o), false, "a pane scrubbed into history has nothing to gain from a retune");
  assert.equal(paneRetuneAction(o), null, "…and a blocked control must not become a device action");
  assert.match(offerLabel(o), /frozen behind the growing edge/);
  // The block is a fact about the VIEW, not about the front end: the plan itself is fine, and says
  // so. Folding it into a `RetunePlan` reason would make those reasons mean two kinds of thing.
  assert.equal(o.plan.ok, true);
});

test("an unreported grid is stated, not silent: not knowing the front end's range is not evidence it can reach here", () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const o = offerFor(m.get(p)!, T0, WINDOWS, null);
  assert.equal(o.plan.ok, false);
  if (o.plan.ok) return;
  assert.equal(o.plan.reason, "no_grid");
  assert.equal(offerAcceptable(o), false);
  assert.match(offerLabel(o), /has not reported a tunable range/);
});

// ---------------------------------------------------------------------------
// 2. The explicit act, through the one gate
// ---------------------------------------------------------------------------

test("taking the offer retunes through T-343's gate: ONE window post carrying both halves", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);          // ~400.8 MHz, nowhere near the tuned window
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  const { ctx, calls } = deviceSpyCtx();
  const out = await acceptPaneRetune(ctx, site, offer);
  assert.equal(out.ok, true);
  // T-529: ONE request, not a rate post followed by a centre post. Two posts commanded two windows
  // and the first of them — the old centre at the new rate — is one nobody asked for.
  assert.deepEqual(calls.map((c) => c.path), ["/api/control/window"]);
  assert.equal((calls[0].body as { sample_rate_hz: number }).sample_rate_hz, 2_000_000,
    "the narrowest covering window, not the one in force");
  const posted = (calls[0].body as { center_hz: number }).center_hz;
  // On the achievable grid (T-341), and it is the plan's own centre — re-snapping it in the gate is
  // a no-op, so the number on the button is the number that goes out.
  assert.ok(offer.plan.ok);
  assert.equal(posted, offer.plan.ok ? offer.plan.centerHz : NaN);
  assert.ok(Math.abs(posted / STEP - Math.round(posted / STEP)) < 1e-6, `${posted} is off the synthesiser grid`);
  // The radio it moved is named from the backend's own state, not claimed by the client. T-498: the
  // toast also names the span the retune actually committed — 2.000 MHz, the narrowest covering
  // window asserted above (`calls[0].body`), not merely "a retune happened" with the width silent.
  assert.match(
    ctx.store.get().toast.text,
    /Retuning hackrf:0000000000000000fake0000000000ab to [\d.]+ MHz, 2\.000 MHz wide/,
  );
});

// T-529: the client no longer decides whether the rate "needs" posting. It used to compare the
// plan's span against its own copy of `device.sampleRateHz` and drop the rate post when they
// matched — a plan, built from a polled value, about how many device actions one press is. Now it
// states the window and the backend decides what that costs: a rate already in force re-plumbs
// nothing (`hk_api::control`'s window route, asserted in `api_contract.rs`).
test("a window already the right width is still ONE post, and it still names both halves", async () => {
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 400e6, spanHz: 1.8e6 }, spanNs: 20 * S,
  });
  const p = m.list()[0].id;
  const site = siteOver(m);
  const { ctx, calls } = deviceSpyCtx();
  ctx.store.set((s) => ({ device: { ...s.device, sampleRateHz: 2_000_000 } }));
  const out = await acceptPaneRetune(ctx, site, site.offerNow(p)!);
  assert.equal(out.ok, true);
  assert.deepEqual(calls.map((c) => c.path), ["/api/control/window"],
    "one press, one device action, whatever the rate in force happens to be");
  assert.equal((calls[0].body as { sample_rate_hz: number }).sample_rate_hz, 2_000_000,
    "the window is stated whole; whether that is a change is the backend's to know");
});

test("a replay is told so, and reaches nothing: the offer is still not a second path to the device", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const { ctx, calls } = deviceSpyCtx(false);
  const out = await acceptPaneRetune(ctx, site, site.offerNow(p)!);
  assert.deepEqual(out, { ok: false, reason: "refused" });
  assert.deepEqual(calls, [], "not live: nothing may be posted");
  assert.equal(site.invalidations, 0, "and nothing is invalidated, because nothing was retuned");
});

// ---------------------------------------------------------------------------
// 3. Snapping and off-DC placement are REUSED
// ---------------------------------------------------------------------------

test("the planned centre is on the grid and places the pane OFF DC, at T-418's derived span/4", () => {
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 433.92e6, spanHz: 200e3 }, spanNs: 20 * S,
  });
  const o = offerFor(m.list()[0])!;
  assert.ok(o.plan.ok);
  if (!o.plan.ok) return;
  // The narrowest achievable window (the 2 Msps floor), because detail comes from resolution, not
  // from an impossible narrow capture.
  assert.equal(o.plan.spanHz, 2e6);
  // The ideal offset is span/4; the pane is far narrower than the window, so the ideal is reachable
  // and only the snap's half step separates the two.
  assert.ok(Math.abs(Math.abs(o.plan.dcOffsetHz) - o.plan.spanHz / 4) <= STEP / 2 + 1e-6,
    `off DC by ${o.plan.dcOffsetHz}, not ~${o.plan.spanHz / 4}`);
  assert.equal(o.plan.clearsDc, true, "DC must fall outside the pane, not on it");
  assert.equal(o.plan.snappedCenter, true);
  assert.ok(Math.abs(o.plan.centerHz / STEP - Math.round(o.plan.centerHz / STEP)) < 1e-6);
  // The window holds the whole pane wherever the dodge put it.
  assert.equal(o.shortfallHz, 0);
  assert.match(offerLabel(o), /^Retune to 433\.\d+ MHz at 2\.000 MHz span \(clear of DC/);
});

test("the offer module re-derives no RF arithmetic of its own: it calls the planner", () => {
  const src = readFileSync("src/surface/retune.ts", "utf8");
  for (const route of ["/api/control/center", "/api/control/rate", "/api/control/window", "/api/control/gains"]) {
    assert.ok(!src.includes(route), `retune.ts must not name ${route}: the one path is applyDeviceAction`);
  }
  // The planner is imported, and neither the tuning step nor the quarter-band offset is restated.
  assert.ok(/import \{[^}]*retunePlan/.test(src), "the capture configuration must come from retunePlan");
  const code = src.split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  // (`1e6`/`1e3` survive: turning Hz into MHz for a label is presentation, not an RF fact.)
  assert.ok(!/28\.61|30e6|2 \*\* 20|2_000_000|20e6/.test(code), "an RF constant appeared in retune.ts");
  assert.ok(!/(dcOffset|snapCenter|smallestCoveringSpan)\s*[=(]/.test(code),
    "the placement and snapping arithmetic was re-derived instead of being left to retunePlan");
});

// ---------------------------------------------------------------------------
// 4. The band edge, and the moved pane
// ---------------------------------------------------------------------------

test("past the band edge the offer is DISABLED with its reason, never clamped to a nearer centre", async () => {
  // A pane whose centre is beyond the top of the device's tunable range. `snapCenter` would walk it
  // inward and call 6 GHz achievable; `containsCenter` is what refuses, and it is the reason T-392
  // kept them separate.
  const grid: FrequencyGrid = { ...GRID, ranges_hz: [[1e6, 500e6]] };
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 700e6, spanHz: 1e6 }, spanNs: 20 * S,
  });
  const p = m.list()[0];
  const o = offerFor(p, T0, WINDOWS, grid)!;
  assert.ok(o, "the pane is un-tuned, so the offer exists — it is the ACTION that is refused");
  assert.equal(o.plan.ok, false);
  if (o.plan.ok) return;
  assert.equal(o.plan.reason, "center_out_of_range");
  assert.equal(offerAcceptable(o), false);
  assert.equal(paneRetuneAction(o), null, "a refusal must not become a smaller retune");
  assert.match(offerLabel(o), /Outside the front end's tunable range/);
  const { ctx, calls } = deviceSpyCtx();
  const site: PaneRetuneSite = { offerNow: () => o, invalidateEdge: () => 0 };
  assert.deepEqual(await acceptPaneRetune(ctx, site, o), { ok: false, reason: "not_acceptable" });
  assert.deepEqual(calls, [], "a disabled offer must reach nothing, even when pressed");
});

test("a pane wider than one capture window is survey overview: stated as such, and not retunable", () => {
  const m = new PaneModel({
    bounds: BOUNDS, lattice: LAT, width: 1200, height: 800,
    freq: { centerHz: 3e9, spanHz: 400e6 }, spanNs: 20 * S,
  });
  const o = offerFor(m.list()[0])!;
  assert.equal(o.plan.ok, false);
  if (o.plan.ok) return;
  assert.equal(o.plan.reason, "span_too_wide");
  assert.match(offerLabel(o), /survey overview/);
});

test("T-407's failure mode, structurally: a pane that MOVED refuses rather than retuning elsewhere", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  // The finger keeps dragging while the offer sits under it — the moving-target version of T-407's
  // "a stroke across the bar counted as travel along it".
  m.panFreq(p, 500e6);
  const { ctx, calls } = deviceSpyCtx();
  assert.deepEqual(await acceptPaneRetune(ctx, site, offer), { ok: false, reason: "moved" });
  assert.deepEqual(calls, [], "the radio went where the button never said");
  assert.equal(site.invalidations, 0);
  // The *fresh* offer is takeable: the refusal is about staleness, not about the pane.
  assert.equal((await acceptPaneRetune(ctx, site, site.offerNow(p)!)).ok, true);
});

test("T-476/T-407: a pane FROZEN between the paint and the press refuses, and reaches nothing", async () => {
  // The hole the persistent control opens, and the reason `sameTarget` compares `block`. Under
  // T-444 a pane scrubbed into the past made `offerNow` return `null`, which read as "moved". Now it
  // returns a well-formed offer whose PLAN is identical — same centre, same span — and only `block`
  // differs. Compare on the plan alone and a frozen viewport retunes the radio.
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  m.panTime(p, -60 * S);                 // pause + scrub, between the label being painted and the press
  const after = site.offerNow(p)!;
  assert.ok(offer.plan.ok && after.plan.ok);
  if (offer.plan.ok && after.plan.ok) {
    assert.equal(after.plan.centerHz, offer.plan.centerHz, "the run needs the PLANS to agree, or it proves nothing");
    assert.equal(after.plan.spanHz, offer.plan.spanHz);
  }
  assert.equal(after.block, "past");
  const { ctx, calls } = deviceSpyCtx();
  assert.deepEqual(await acceptPaneRetune(ctx, site, offer), { ok: false, reason: "moved" });
  assert.deepEqual(calls, [], "a frozen viewport commanded the radio");
  assert.equal(site.invalidations, 0);
});

test("T-476 THE COMMIT: what goes out is the CONTAINED viewport as it stands at the instant of commit", async () => {
  // The failure mode this repo keeps hitting, applied here: *a control that renders is not a control
  // that retunes to the viewport it is showing.* So this asserts the bodies actually POSTed, against
  // the pane's own window read at commit — not against the offer object, which would be comparing
  // the arithmetic to itself.
  const m = model();                                    // 100.8 MHz ± 500 kHz, inside the window
  const p = m.list()[0].id;
  m.zoomFreq(p, 0.2);                                   // …zoomed to 200 kHz, still inside it
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  const { ctx, calls } = deviceSpyCtx();
  const out = await acceptPaneRetune(ctx, site, offer);
  assert.equal(out.ok, true);

  // Re-derive the target from the PANE, through the planner, with no reference to `offer`.
  const box = m.get(p)!.freq;
  const expected = retunePlan(GRID, box.centerHz - box.spanHz / 2, box.centerHz + box.spanHz / 2);
  assert.ok(expected.ok);
  if (!expected.ok) return;
  const posted = Object.fromEntries(calls.map((c) => [c.path, c.body]));
  assert.deepEqual(posted["/api/control/window"],
    { center_hz: expected.centerHz, sample_rate_hz: expected.spanHz },
    "one window, both halves, exactly as the planner computed them");
  // …and the centre that went out is the OFF-DC placement for this viewport, not the viewport's own
  // midpoint: the plan is being obeyed, not approximated by something that happens to be nearby.
  assert.ok(Math.abs((box.centerHz - expected.centerHz) - expected.spanHz / 4) <= STEP / 2 + 1e-6,
    `${expected.centerHz} is not span/4 below the viewport's ${box.centerHz}`);
  // The resolution claim, measured: the capture that went out is narrower than the one in force.
  assert.ok(expected.spanHz < WINDOWS[0].spanHz, "the retune must buy a narrower capture, or the ticket's premise is wrong");
  // NON-VACUITY: the same run with the pane moved between paint and commit posts NOTHING, so the
  // agreement above is the guard working rather than the guard being absent.
  const m2 = model();
  const q = m2.list()[0].id;
  m2.zoomFreq(q, 0.2);
  const site2 = siteOver(m2);
  const stale = site2.offerNow(q)!;
  m2.panFreq(q, 500e6);
  const spy2 = deviceSpyCtx();
  assert.deepEqual(await acceptPaneRetune(spy2.ctx, site2, stale), { ok: false, reason: "moved" });
  assert.deepEqual(spy2.calls, []);
});

test("a pan too small to change the snapped configuration is not a moved target", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const offer = site.offerNow(p)!;
  m.panFreq(p, STEP / 8); // far below one tuning step: the plan cannot notice
  const { ctx } = deviceSpyCtx();
  assert.equal((await acceptPaneRetune(ctx, site, offer)).ok, true);
});

// ---------------------------------------------------------------------------
// The growing edge is invalidated after a successful retune (T-437 §5.2)
// ---------------------------------------------------------------------------

function tileData(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 1, nt: 1, value: new Float32Array([-90]),
    state: new Uint8Array([CELL.OBSERVED]), tier: "live-iq", answeredLevel: a.levelF,
    fold: { frequency: "exact", time: "exact" }, rangeDb: { lo: -100, hi: -60 },
    bytes: 1024, serverInFlightLimit: null,
  };
}

/** A cache with `n` tile rows loaded, the last of which holds the live edge. */
async function loadedCache(edgeNs: number) {
  const destroyed: string[] = [];
  const asked: TileAddr[] = [];
  const cache = new TileCache<string>(
    { upload: (d) => d.key, destroy: (t) => { destroyed.push(t); } },
    (a) => { asked.push(a); return Promise.resolve(tileData(a)); },
    { inFlight: 64, now: () => 0 },
  );
  const tTile = LAT.t0Ns * LAT.cells;
  const tEdge = Math.floor(edgeNs / tTile);
  const addrs: TileAddr[] = [];
  for (let dt = 0; dt <= 3; dt++) {
    for (const levelT of [0, 2]) {
      const w = LAT.t0Ns * 2 ** levelT * LAT.cells;
      addrs.push({ device: "any", scheme: "view", levelF: 0, levelT, fIndex: 400, tIndex: Math.floor(edgeNs / w) - dt, cells: LAT.cells });
    }
  }
  cache.beginFrame();
  for (const a of addrs) cache.acquire(a);
  cache.endFrame();
  await new Promise((r) => setImmediate(r));
  return { cache, destroyed, asked, addrs, tEdge };
}

test("after a retune the growing edge's tiles are dropped — at every level, not only the finest", async () => {
  const { cache, addrs } = await loadedCache(T0);
  assert.equal(cache.residentTiles, addrs.length, "the run needs every tile resident to prove anything");
  const dropped = cache.invalidateEdge(LAT, T0);
  assert.ok(dropped >= 2, `only ${dropped} edge tiles dropped`);
  // One per level ladder: the edge tile at level_t 0 and the one at level_t 2 both hold the edge.
  assert.equal(cache.residentTiles, addrs.length - dropped);
  // …and the older rows, which describe spectrum the retune cannot rewrite, are kept.
  assert.ok(cache.residentTiles > 0, "invalidating the edge must not clear the whole cache");
});

test("a tile on another lattice is untouched: its scheme is part of its key", async () => {
  const { cache } = await loadedCache(T0);
  const before = cache.residentTiles;
  const other: Lattice = { ...LAT, scheme: "store:1" };
  assert.equal(cache.invalidateEdge(other, T0), 0);
  assert.equal(cache.residentTiles, before);
});

test("a fetch the retune overtook is dropped on arrival and asked for again, not served stale", async () => {
  let fetches = 0;
  let release: ((d: TileData) => void) | null = null;
  // Each answer is stamped with the fetch that produced it, so "the old tuning's tile is on screen"
  // is observable rather than inferred.
  const stamped = (a: TileAddr, n: number): TileData => ({ ...tileData(a), value: new Float32Array([n]) });
  const cache = new TileCache<string>(
    { upload: (d) => d.key, destroy: () => {} },
    (a) => {
      const n = ++fetches;
      return n === 1 ? new Promise<TileData>((res) => { release = () => res(stamped(a, 1)); }) : Promise.resolve(stamped(a, n));
    },
    { inFlight: 4, now: () => 0 },
  );
  const addr: TileAddr = { device: "any", scheme: "view", levelF: 0, levelT: 0, fIndex: 400, tIndex: Math.floor(T0 / (LAT.t0Ns * LAT.cells)), cells: LAT.cells };
  cache.beginFrame();
  cache.acquire(addr);
  cache.endFrame();
  assert.equal(cache.inFlightCount, 1);
  cache.invalidateEdge(LAT, T0);       // the retune lands while the tile is still in flight
  release!();                          // …and the old tuning's answer arrives afterwards
  for (let i = 0; i < 4; i++) await new Promise((r) => setImmediate(r));
  assert.equal(fetches, 2, "the overtaken tile was not re-requested, so the pane would go blank instead");
  assert.equal(cache.peek(addr)?.data.value[0], 2, "the tile of the tuning that just ended was served");
});

test("the invalidation happens ONLY after the front end took the retune", async () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6);
  const site = siteOver(m);
  const { ctx } = deviceSpyCtx();
  const out = await acceptPaneRetune(ctx, site, site.offerNow(p)!);
  assert.equal(out.ok && out.invalidated, 7);
  assert.equal(site.invalidations, 1, "invalidated once, after the retune, and not before it");
});

// ---------------------------------------------------------------------------
// 4. T-496: an explicit capture-width control
// ---------------------------------------------------------------------------
//
// The width control is a second way to ASK for what zoom-then-retune already achieves, so its
// claims mirror T-444/T-476's own: always offered (stated and disabled, never hidden), one
// target-derivation path (through `retunePlan`, never a second copy of the arithmetic), the SNAPPED
// span said alongside the asked one when they differ, and the same T-407 guard against a pane that
// moved between paint and press.

function widthSiteOver(m: PaneModel, edgeNs = T0): PaneWidthSite & { invalidations: number } {
  const site = {
    invalidations: 0,
    offerNow: (id: string, spanHz: number) => {
      const p = m.get(id);
      return p ? paneWidthOffer(p, spanHz, GRID, edgeNs) : null;
    },
    invalidateEdge: () => { site.invalidations++; return 7; },
  };
  return site;
}

test("T-496: paneWidthOffer re-derives no arithmetic of its own — it calls retunePlan, over a region built from the pane's OWN centre", () => {
  const m = model(); // opens at centerHz: 100.8e6
  const p = m.list()[0].id;
  const pane = m.get(p)!;
  for (const askedSpanHz of [500e3, 2e6, 10e6]) {
    const got = paneWidthOffer(pane, askedSpanHz, GRID, T0).plan;
    const want = retunePlan(GRID, pane.freq.centerHz - askedSpanHz / 2, pane.freq.centerHz + askedSpanHz / 2);
    assert.deepEqual(got, want, `askedSpanHz=${askedSpanHz}: paneWidthOffer must not diverge from retunePlan`);
  }
});

test("T-496: an achievable preset states just the one span it will capture", () => {
  const m = model();
  const o = paneWidthOffer(m.get(m.list()[0].id)!, 2e6, GRID, T0); // GRID's own span floor
  assert.ok(o.plan.ok);
  // Not exactly 2e6: `retunePlan`'s off-DC placement (T-418) can widen the covering span past the
  // bare floor by a fraction of a tuning step, which is real and correct — this asserts "close
  // enough to round to the same label", not a number this test would have to re-derive by hand.
  assert.ok(o.plan.ok && Math.abs(o.plan.spanHz - 2e6) < 1e3, `expected ~2 MHz, got ${o.plan.ok && o.plan.spanHz}`);
  assert.equal(widthOfferAcceptable(o), true);
  assert.match(widthOfferLabel(o), /^Capture 2\.000 MHz here$/);
});

test("T-496: widthOfferLabel states the SNAPPED span, and both numbers only when they differ", () => {
  // A pure check of the formatting rule itself, over a synthetic plan — independent of retunePlan's
  // own off-DC arithmetic, which the two tests above already exercise for real.
  const plan = (spanHz: number): RetunePlan => ({
    ok: true, centerHz: 100.8e6, spanHz, snappedCenter: true, source: "live-iq", dcOffsetHz: 0, clearsDc: true,
  });
  const base = { paneId: "p", device: "any", block: null as const };
  assert.equal(
    widthOfferLabel({ ...base, askedSpanHz: 2e6, plan: plan(2e6) }),
    "Capture 2.000 MHz here",
  );
  assert.equal(
    widthOfferLabel({ ...base, askedSpanHz: 500e3, plan: plan(2e6) }),
    "Asked for 0.500 MHz; the narrowest achievable capture is 2.000 MHz",
  );
});

test("T-496: a preset narrower than the front end can go SAYS BOTH numbers — asked and snapped — never just one", () => {
  const m = model();
  const o = paneWidthOffer(m.get(m.list()[0].id)!, 500e3, GRID, T0); // below GRID.spans_hz.min = 2e6
  assert.ok(o.plan.ok);
  assert.equal(o.plan.ok && o.plan.spanHz, 2e6, "floors to the narrowest achievable span");
  assert.equal(widthOfferAcceptable(o), true, "still takeable — it just does not deliver 500 kHz");
  assert.match(widthOfferLabel(o), /^Asked for 0\.500 MHz; the narrowest achievable capture is 2\.000 MHz$/);
});

test("T-496: a preset wider than one live window is stated and disabled, never hidden", () => {
  const m = model();
  const o = paneWidthOffer(m.get(m.list()[0].id)!, 25e6, GRID, T0); // > GRID.max_live_span_hz = 20e6
  assert.equal(o.plan.ok, false);
  assert.equal(widthOfferAcceptable(o), false);
  assert.equal(paneWidthAction(o), null, "a disabled preset must not become a device action");
  assert.match(widthOfferLabel(o), /survey overview/);
});

test("T-496: a pane frozen behind the growing edge is stated and disabled, exactly as the retune control is", () => {
  const m = model();
  const p = m.list()[0].id;
  m.panTime(p, -60 * S);
  const o = paneWidthOffer(m.get(p)!, 2e6, GRID, T0);
  assert.equal(o.block, "past");
  assert.equal(widthOfferAcceptable(o), false);
  assert.match(widthOfferLabel(o), /frozen behind the growing edge/);
});

test("T-496: taking a preset reaches the device exactly once, named pane-width, at the SAME centre a plain retune would pick", () => {
  const m = model();
  const p = m.list()[0].id;
  m.panFreq(p, 300e6); // an arbitrary pane position — the width control does not care where it is
  const site = widthSiteOver(m);
  const offer = site.offerNow(p, 2e6)!;
  const { ctx, calls } = deviceSpyCtx(); // dev.sampleRateHz = 2_400_000, so the 2 MHz preset re-posts the rate
  return acceptPaneWidth(ctx, site, offer).then((out) => {
    assert.equal(out.ok, true);
    assert.equal(out.ok && out.action.source, "pane-width", "tellable apart from a plain retune in the audit trail");
    assert.deepEqual(calls.map((c) => c.path), ["/api/control/window"]);
    // The posted rate is `Math.round(plan.spanHz)`, not a re-derivation of it — asserted against the
    // offer's own plan rather than a literal 2_000_000, since T-418's off-DC placement can widen the
    // achievable span past the bare 2 MHz floor by a fraction of a tuning step (see the label tests).
    assert.ok(offer.plan.ok);
    assert.equal((calls[0].body as { sample_rate_hz: number }).sample_rate_hz,
      offer.plan.ok ? Math.round(offer.plan.spanHz) : NaN);
    // T-498, applied to this control too: the toast names the span it actually committed.
    const wideMhz = offer.plan.ok ? (offer.plan.spanHz / 1e6).toFixed(3) : "?";
    assert.match(
      ctx.store.get().toast.text,
      new RegExp(`Retuning hackrf:0000000000000000fake0000000000ab to [\\d.]+ MHz, ${wideMhz.replace(".", "\\.")} MHz wide`),
    );
    assert.equal(out.ok && out.invalidated, 7, "the growing edge is invalidated after a successful width change too");
  });
});

test("T-496/T-407: a pane that moved between paint and press refuses, and reaches nothing", async () => {
  const m = model();
  const p = m.list()[0].id;
  const site = widthSiteOver(m);
  const offer = site.offerNow(p, 2e6)!;
  m.panFreq(p, 250e6); // the pane moves after the offer was painted, before the press
  const { ctx, calls } = deviceSpyCtx();
  const out = await acceptPaneWidth(ctx, site, offer);
  assert.deepEqual(out, { ok: false, reason: "moved" });
  assert.deepEqual(calls, [], "a stale offer must reach nothing");
});
