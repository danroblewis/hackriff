// T-155: Review drawer (ADR-0013 §2, §8). No DOM under node:test, so only the pure view-model,
// query-default and store-action functions are tested directly (the same technique as
// app-shell.test.ts and the old alarms/report/scheduler tests); the DOM-wiring classes
// (AlarmsTab, ReportTab, HistoryTab, SchedulerTab, DeviceTab, BookmarksTab) are exercised manually
// against a running `hk serve`, like every other MUI panel.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { AnomalyRow } from "../src/alarms";
import { alarmEmitterId, alarmRowView } from "../src/app/review/alarms";
import { newBookmarkFromForm } from "../src/app/review/bookmarks";
import { defaultReportParams } from "../src/app/review/report";
import {
  bookmarksError, openReview, reviewInitial, REVIEW_TABS, SETTINGS_TABS, setBookmarks, tabGroup,
  toggleReview,
} from "../src/app/review/slice";
import type { AppState } from "../src/app/state";

function mkAnomaly(over: Partial<AnomalyRow> & { status: AnomalyRow["status"] }): AnomalyRow {
  return {
    id: "a1", kind: "new-emitter", subject: {}, f_lo: 433e6, f_hi: 433.1e6, t0: 0, t1: 1, t: 1, score: 0.9,
    alarm: { key: { kind: "new-emitter", site: "site-7", subject: {} }, state: "open", last_transition: "raised", raised_at: 0, last_t: 1, reopen_count: 0, f_lo: 433e6, f_hi: 433.1e6, detail: {} },
    explanations: [{ id: "e1", cause: { kind: "emitter" }, score: 0.7, provisional: false, t: 1 }],
    ...over,
  };
}

// ---- slice: tab open/close, region carried, bookmarks mirror ----

test("reviewInitial starts closed on the alarms tab with no region and an empty bookmark list", () => {
  const s = reviewInitial();
  assert.deepEqual(s.review, { open: false, tab: "alarms", region: null });
  assert.deepEqual(s.bookmarks, { list: [], loadedAtS: null, error: null });
});

test("toggleReview flips open without touching a review tab or the region", () => {
  const s0: Pick<AppState, "review"> = { review: { open: false, tab: "report", region: { loHz: 1, hiHz: 2 } } };
  const p1 = toggleReview(s0 as unknown as AppState);
  assert.deepEqual(p1.review, { open: true, tab: "report", region: { loHz: 1, hiHz: 2 } });
});

// T-1007: the drawer is shared with the settings panels, so the REVIEW button must open the review
// group — the badge it carries counts alarms, and reopening onto the device controls would put
// settings under it. Closing keeps the tab, so close/open returns where you were.
test("toggleReview opens on the review group when the drawer last showed a settings panel", () => {
  const s0 = { review: { open: false, tab: "device" as const, region: null } } as unknown as AppState;
  assert.deepEqual(toggleReview(s0).review, { open: true, tab: "alarms", region: null });
  // Open on a SETTINGS panel, Review switches to alarms rather than closing: the badge's button must
  // not shut a drawer that is showing something else, or the count it carries becomes unreachable.
  const onSettings = { review: { open: true, tab: "device" as const, region: null } } as unknown as AppState;
  assert.deepEqual(toggleReview(onSettings).review, { open: true, tab: "alarms", region: null });
  // Open on review, it is the toggle it has always been.
  const onReview = { review: { open: true, tab: "alarms" as const, region: null } } as unknown as AppState;
  assert.deepEqual(toggleReview(onReview).review, { open: false, tab: "alarms", region: null });
});

test("T-1007: every tab belongs to exactly one group, and nothing configurable is in Review", () => {
  assert.deepEqual([...REVIEW_TABS], ["alarms", "report"]);
  assert.deepEqual([...SETTINGS_TABS], ["device", "scheduler", "bookmarks"]);
  for (const t of REVIEW_TABS) assert.equal(tabGroup(t), "review");
  for (const t of SETTINGS_TABS) assert.equal(tabGroup(t), "settings");
  // The drawer names the group it is showing, and the tab bar hides the other group's tabs.
  const src = readFileSync("src/app/review/drawer.ts", "utf8");
  assert.match(src, /title\.textContent = GROUP_TITLE\[group\];/);
  assert.match(src, /btn\.hidden = tabGroup\(id\) !== group;/);
});

test("openReview sets open, tab and region (a selection's History action)", () => {
  const region = { loHz: 433e6, hiHz: 434e6, t0: 10, t1: 20 };
  const p = openReview("report", region)();
  assert.deepEqual(p.review, { open: true, tab: "report", region });
});

test("openReview defaults region to null", () => {
  assert.deepEqual(openReview("device")().review, { open: true, tab: "device", region: null });
});

test("setBookmarks replaces the mirrored list and clears any error", () => {
  const s0: Pick<AppState, "bookmarks"> = { bookmarks: { list: [], loadedAtS: null, error: "boom" } };
  const list = [{ id: "b1", kind: "marker" as const, name: "x", f_center_hz: 1e6, bandwidth_hz: null, note: null, created_s: 0, updated_s: 0 }];
  const p = setBookmarks(list)();
  assert.equal(p.bookmarks!.list, list);
  assert.equal(p.bookmarks!.error, null);
  void s0;
});

test("bookmarksError keeps the list and sets the error", () => {
  const s0 = { bookmarks: { list: [], loadedAtS: 5, error: null } } as unknown as AppState;
  const p = bookmarksError("offline")(s0);
  assert.deepEqual(p.bookmarks, { list: [], loadedAtS: 5, error: "offline" });
});

// ---- alarms: row view-model and emitter-id extraction (Focus in Explore) ----

test("alarmEmitterId reads a plain string subject; anything else is null, never guessed", () => {
  assert.equal(alarmEmitterId({ subject: "em-1" }), "em-1");
  assert.equal(alarmEmitterId({ subject: { cell: 1 } }), null);
  assert.equal(alarmEmitterId({ subject: null }), null);
});

test("alarmRowView formats kind, channel, site, explanation and dismiss/reopen eligibility", () => {
  const v = alarmRowView(mkAnomaly({ status: "open", subject: "em-9" }));
  assert.equal(v.kind, "new-emitter");
  assert.equal(v.channel, "433.0000–433.1000 MHz");
  assert.equal(v.site, "site-7");
  assert.match(v.explanation, /emitter/);
  assert.equal(v.canDismiss, true);
  assert.equal(v.canReopen, false);
  assert.equal(v.emitterId, "em-9");
});

test("alarmRowView shows a dash site for a floor episode (no alarm) and offers reopen once dismissed", () => {
  const noAlarm = alarmRowView(mkAnomaly({ status: "open", alarm: null }));
  assert.equal(noAlarm.site, "—");
  assert.equal(noAlarm.canDismiss, false);
  const dismissed = alarmRowView(mkAnomaly({
    status: "dismissed",
    alarm: { key: { kind: "new-emitter", site: "site-1", subject: {} }, state: "dismissed", last_transition: "dismissed", raised_at: 0, last_t: 1, reopen_count: 0, f_lo: 433e6, f_hi: 433.1e6, detail: {} },
  }));
  assert.equal(dismissed.canReopen, true);
});

// ---- report tab: default region/span from already-known UI state ----

test("defaultReportParams uses the drawer's region when the drawer was opened on one", () => {
  const p = defaultReportParams({ loHz: 100e6, hiHz: 101e6, t0: 1000, t1: 2000 }, { view: null }, { live: true }, 5000);
  assert.deepEqual(p, { fLoHz: 100e6, fHiHz: 101e6, t0: 1000, t1: 2000 });
});

test("defaultReportParams falls back to the live view and the last hour up to now", () => {
  const p = defaultReportParams(null, { view: { loHz: 90e6, hiHz: 92e6 } }, { live: true }, 10_000);
  assert.deepEqual(p, { fLoHz: 90e6, fHiHz: 92e6, t0: 10_000 - 3600, t1: 10_000 });
});

test("defaultReportParams uses the reviewed instant, not now, while reviewing", () => {
  const p = defaultReportParams(null, { view: { loHz: 1e6, hiHz: 2e6 } }, { live: false, tS: 500 }, 10_000);
  assert.equal(p!.t1, 500);
  assert.equal(p!.t0, 500 - 3600);
});

test("defaultReportParams is null with no region and no live view (nothing to guess)", () => {
  assert.equal(defaultReportParams(null, { view: null }, { live: true }, 1), null);
});

// ---- history tab: default region widens the span by 10 s either side ----

// T-445 retired the drawer's "Spectrum grid" tab — the region-over-time waterfall over
// `GET /api/history`, with its own hand-written colormap LUT (T-397's colormap divergence, in the
// repo, twice). Its two tests went with it: `defaultHistoryRegion` chose the tab's opening region,
// and there is no tab to open. The question it answered is the unified surface's whole subject.

test("newBookmarkFromForm names an unnamed marker after its frequency and omits bandwidth when unset", () => {
  const b = newBookmarkFromForm({ name: "", freqMHz: "101.3", bandwidthKHz: "", kind: "marker" });
  assert.equal(b!.name, "101.3 MHz");
  assert.equal(b!.bandwidth_hz, undefined);
});

test("newBookmarkFromForm is null for an unreadable frequency", () => {
  assert.equal(newBookmarkFromForm({ name: "x", freqMHz: "", bandwidthKHz: "", kind: "marker" }), null);
  assert.equal(newBookmarkFromForm({ name: "x", freqMHz: "not a number", bandwidthKHz: "", kind: "marker" }), null);
  assert.equal(newBookmarkFromForm({ name: "x", freqMHz: "-5", bandwidthKHz: "", kind: "marker" }), null);
});

// ---- layout: the drawer goes full-screen and its wide table scrolls sideways at narrow widths ----

test("review.css/base.css: the drawer is full-screen under 900px, and the survey table is its own scroller", () => {
  const css = readFileSync("src/app/review/review.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.match(css, /\.rv-table\s*\{[^}]*display:\s*block;\s*overflow-x:\s*auto/, "the wide survey table gets its own scroller, not the page");
  assert.match(css, /@media \(max-width:\s*900px\)/);
  for (const m of css.matchAll(/min-width:\s*(\d+)px/g)) assert.ok(Number(m[1]) <= 400);
  assert.match(readFileSync("src/app/base.css", "utf8"), /@media \(max-width:\s*900px\)\s*\{[\s\S]*\.review\s*\{[^}]*top:\s*0;\s*bottom:\s*0/);
});
