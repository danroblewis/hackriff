// T-1007 (MMAP): the ⋯ settings menu takes over from Review, whose configurable panels move into it.
//
// User, 2026-09-25: "instead of the Review section a lot of those things could be considered
// settings." The claims, each against what would make a degenerate implementation pass:
//
//  1. **Nothing configurable is left in Review.** The drawer's tabs are two groups, the review group
//     is alarms + the survey report, and the drawer shows one group at a time (`app-review.test.ts`
//     owns the group split; here: the ⋯ menu is the home the moved settings landed in).
//  2. **Every moved setting is really in the one small menu**, with its own group and its rows
//     derived from the state in force — never tracked beside it, so a press cannot leave two rows on.
//  3. **The ruler mode is a LABEL form**, not a second set of marks: the same ticks come back at the
//     same values and instants, relabelled from seconds-ago to the instant's own clock time (driven
//     over the real `paneRuler` + `hudLabels`, so "the setting does nothing" cannot pass). The mode is
//     T-998's `TimeLabelMode`; this menu is where it is chosen, never a second copy of it.
//  4. **Thin client**: the whole settings path — the model, every press, the host that implements it
//     — names no route, no client and no device action. The capture window is *stated* from the
//     backend's own `window`, and there is no route to change a retention, so the menu says where it
//     is set rather than implying a control (the honesty rule, applied to a setting).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { rulerRows, type SettingsRow } from "../src/app/chrome/settings";
import { fmtRulerLocal, hudLabels, paneRuler } from "../src/surface/hud";
import type { PaneRect } from "../src/surface/surface";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOX = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

const on = (rows: SettingsRow[]) => rows.filter((r) => r.on).map((r) => r.id);

// ---- (2) the rows are derived from the mode in force ----

test("the ruler rows offer seconds-ago and clock time, exactly one lit, derived from the mode", () => {
  assert.deepEqual(rulerRows("relative").map((r) => r.id), ["relative", "absolute"]);
  assert.deepEqual(on(rulerRows("relative")), ["relative"]);
  assert.deepEqual(on(rulerRows("absolute")), ["absolute"]);
  for (const m of ["relative", "absolute"] as const) for (const r of rulerRows(m)) assert.ok(r.label && r.hint, "a row with no words");
});

// ---- (3) the ruler mode swaps the LABELS of the same marks ----

test("the clock-time ruler keeps every mark and its instant, and only relabels it", () => {
  // T-998 owns the label forms (the narrow ruler); T-1007's claim is that the setting relabels the
  // SAME marks. One ruler, laid out once, labelled both ways.
  const r = paneRuler("p", BOX, RECT, 1e3, 0.01, T0, 1);
  const age = hudLabels(r, RECT.h, 1, null, "relative");
  const clock = hudLabels(r, RECT.h, 1, null, "absolute");
  assert.deepEqual(clock.map((l) => [l.axis, l.value, l.x, l.y]), age.map((l) => [l.axis, l.value, l.x, l.y]),
    "the clock mode moved, added or dropped a mark: it may only relabel");
  assert.deepEqual(clock.filter((l) => l.axis === "freq"), age.filter((l) => l.axis === "freq"),
    "the frequency ruler is not the time ruler's setting");
  const ageT = age.filter((l) => l.axis === "time"), clockT = clock.filter((l) => l.axis === "time");
  assert.ok(ageT.length >= 2, "a 20 s pane over 500 px has labelled marks to compare");
  for (const [i, l] of clockT.entries()) {
    assert.equal(l.text, fmtRulerLocal(l.value, r.timeStepNs), "the clock mode does not read the mark's own instant");
    // Non-vacuity: the two forms really are different strings on a labelled mark.
    assert.notEqual(l.text, ageT[i].text);
  }
});

// ---- (1)/(2) the menu is the home, and it holds every setting the ticket moved ----

test("the ⋯ settings menu holds theme, colour scale, ruler, front ends and the capture window", () => {
  const menu = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  // One small button, and the menu is rendered by `settings.ts` when it opens.
  assert.match(menu, /class: "map-ibtn map-more-btn"/);
  assert.match(menu, /renderSettings\(settingsList, host, \(\) => setMoreOpen\(false\)\)/);
  // Theme is still the moved node itself (T-993), inside this menu.
  assert.match(menu, /registerMapHome\(\{[^}]*more: moreBody[^}]*\}\)/);
  const src = readFileSync("src/app/chrome/settings.ts", "utf8");
  for (const axis of ["scale", "ruler", "devices", "capture"]) {
    assert.match(src, new RegExp(`group\\("${axis}"`), `the settings menu has no ${axis} group`);
  }
  // The pane's coverage source is a select per pane, over `any` plus the front ends.
  assert.match(src, /"data-pane-device": p\.id/);
  assert.match(src, /host\.setDeviceOfPane\(p\.id, sel\.value\)/);
});

test("the settings host is view state only: no route, no client, no device action", () => {
  const src = readFileSync("src/app/chrome/settings.ts", "utf8");
  // Comments cite the routes the words come FROM (that is the point of them); code must not.
  const code = src.split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  for (const forbidden of [/["'`][^"'`]*\/api\//, /\bfetch\s*\(/, /client\./, /DeviceAction/]) {
    assert.doesNotMatch(code, forbidden, `the settings menu names ${forbidden}`);
  }
  // The host's own implementation: the scale is the surface's range mode, the ruler is T-998's
  // label preference, and a pane's device is `PaneModel.setDevice` — three writes, none of them a call.
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(host, /setRulerMode: \(mode\) => \{[\s\S]*?setTimeLabelMode\(mode\);/);
  assert.match(host, /setDeviceOfPane: \(paneId, device\) => \{[\s\S]*?pv\.view\.panes\.setDevice\(paneId, device\);/);
  // The capture window is READ from the store's own capture clock (`GET /api/timeline`'s `window`),
  // and a null one is stated as unknown — never a default span (T-379).
  assert.match(host, /retention: w \? durationText\(w\.spanS\) : null/);
  assert.match(host, /never a default span/);
  // There is no retention route to offer, so the menu says where the setting lives instead.
  assert.match(host, /--iq-retention/);
});

test("the front-end list is the backend's, and an empty one is a replay — never one invented device", () => {
  const shell = readFileSync("src/app/shell.ts", "utf8");
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  // Integration with T-1006: the settings menu's list is the store's `frontEnds` (T-1006's
  // addressable-only reading of the same wire list is `devices`), still mapped from cs.devices.
  assert.match(shell, /frontEnds: \(cs\.devices \?\? \[\]\)\.map/, "the list is not read from /api/control/state's devices");
  assert.doesNotMatch(shell, /(devices|frontEnds): cs\.devices \?\? \[\{/, "a device list is synthesised from the singular device");
  assert.match(host, /const devices = store\.get\(\)\.device\.frontEnds;/, "the settings menu does not read the full front-end list");
  assert.match(host, /empty: devices\.length === 0/);
  assert.match(host, /this run is a replay/);
});
