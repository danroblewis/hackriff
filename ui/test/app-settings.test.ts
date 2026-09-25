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
//     same values and instants, with the primary and secondary strings swapped (driven over the real
//     `paneRuler`, so "the setting does nothing" cannot pass).
//  4. **Thin client**: the whole settings path — the model, every press, the host that implements it
//     — names no route, no client and no device action. The capture window is *stated* from the
//     backend's own `window`, and there is no route to change a retention, so the menu says where it
//     is set rather than implying a control (the honesty rule, applied to a setting).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { rulerRows, type SettingsRow } from "../src/app/chrome/settings";
import { paneRuler } from "../src/surface/hud";
import type { PaneRect } from "../src/surface/surface";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const BOX = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - 20 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

const on = (rows: SettingsRow[]) => rows.filter((r) => r.on).map((r) => r.id);

// ---- (2) the rows are derived from the mode in force ----

test("the ruler rows offer seconds-ago and timestamp, exactly one lit, derived from the mode", () => {
  assert.deepEqual(rulerRows("age").map((r) => r.id), ["age", "clock"]);
  assert.deepEqual(on(rulerRows("age")), ["age"]);
  assert.deepEqual(on(rulerRows("clock")), ["clock"]);
  for (const m of ["age", "clock"] as const) for (const r of rulerRows(m)) assert.ok(r.label && r.hint, "a row with no words");
});

// ---- (3) the ruler mode swaps the LABELS of the same marks ----

test("the timestamp ruler keeps every mark and its instant, and swaps the two strings", () => {
  const age = paneRuler("p", BOX, RECT, 1e3, 0.01, T0, 1, "age");
  const clock = paneRuler("p", BOX, RECT, 1e3, 0.01, T0, 1, "clock");
  assert.deepEqual(clock.time.map((t) => [t.value, t.pos, t.major]), age.time.map((t) => [t.value, t.pos, t.major]),
    "the timestamp mode moved, added or dropped a mark: it may only relabel");
  assert.deepEqual(clock.freq, age.freq, "the frequency ruler is not the time ruler's setting");
  const major = age.time.filter((t) => t.major);
  assert.ok(major.length >= 2, "a 20 s pane over 500 px has labelled marks to compare");
  for (const [i, t] of age.time.entries()) {
    assert.equal(clock.time[i].label, t.sub, "the timestamp mode does not read the UTC instant");
    assert.equal(clock.time[i].sub, t.label, "the age is not kept beside it");
  }
  // Non-vacuity: the two forms really are different strings on a labelled mark.
  assert.notEqual(major[0].label, major[0].sub);
  assert.match(String(major[0].sub), /Z$/, "the secondary is not a UTC instant");
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
  assert.match(src, /host\.setPaneDevice\(p\.id, sel\.value\)/);
});

test("the settings host is view state only: no route, no client, no device action", () => {
  const src = readFileSync("src/app/chrome/settings.ts", "utf8");
  // Comments cite the routes the words come FROM (that is the point of them); code must not.
  const code = src.split("\n").filter((l) => !/^\s*(\/\/|\*|\/\*)/.test(l)).join("\n");
  for (const forbidden of [/["'`][^"'`]*\/api\//, /\bfetch\s*\(/, /client\./, /DeviceAction/]) {
    assert.doesNotMatch(code, forbidden, `the settings menu names ${forbidden}`);
  }
  // The host's own implementation: the scale is the surface's range mode, the ruler is the view's
  // label form, and a pane's device is `PaneModel.setDevice` — three writes, none of them a call.
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(host, /setRulerMode: \(mode\) => \{[\s\S]*?pv\.view\.rulerMode = mode;[\s\S]*?saveRulerMode\(mode\);/);
  assert.match(host, /setPaneDevice: \(paneId, device\) => \{[\s\S]*?pv\.view\.panes\.setDevice\(paneId, device\);/);
  // The capture window is READ from the store's own capture clock (`GET /api/timeline`'s `window`),
  // and a null one is stated as unknown — never a default span (T-379).
  assert.match(host, /retention: w \? durationText\(w\.spanS\) : null/);
  assert.match(host, /never a default span/);
  // There is no retention route to offer, so the menu says where the setting lives instead.
  assert.match(host, /--iq-retention/);
});

test("the front-end list is the backend's, and an empty one is a replay — never one invented device", () => {
  const shell = readFileSync("src/app/shell.ts", "utf8");
  assert.match(shell, /devices: \(cs\.devices \?\? \[\]\)\.map/, "the list is not read from /api/control/state's devices");
  assert.doesNotMatch(shell, /devices: cs\.devices \?\? \[\{/, "a device list is synthesised from the singular device");
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(host, /empty: devices\.length === 0/);
  assert.match(host, /this run is a replay/);
});
