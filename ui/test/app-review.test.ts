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
import { defaultHistoryRegion } from "../src/app/review/history";
import { bookmarksError, openReview, reviewInitial, setBookmarks, toggleReview } from "../src/app/review/slice";
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

test("toggleReview flips open without touching tab or region", () => {
  const s0: Pick<AppState, "review"> = { review: { open: false, tab: "history", region: { loHz: 1, hiHz: 2 } } };
  const p1 = toggleReview(s0 as unknown as AppState);
  assert.deepEqual(p1.review, { open: true, tab: "history", region: { loHz: 1, hiHz: 2 } });
});

test("openReview sets open, tab and region (a selection's History action)", () => {
  const region = { loHz: 433e6, hiHz: 434e6, t0: 10, t1: 20 };
  const p = openReview("history", region)();
  assert.deepEqual(p.review, { open: true, tab: "history", region });
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

test("defaultHistoryRegion widens a region's span by 10 s and keeps its extent", () => {
  const r = defaultHistoryRegion({ loHz: 1e6, hiHz: 2e6, t0: 100, t1: 200 }, { view: null }, { live: true }, 9999);
  assert.deepEqual(r, { fLoHz: 1e6, fHiHz: 2e6, t0: 90, t1: 210 });
});

test("defaultHistoryRegion falls back to the live view and the last 10 minutes", () => {
  const r = defaultHistoryRegion(null, { view: { loHz: 5e6, hiHz: 6e6 } }, { live: true }, 1000);
  assert.deepEqual(r, { fLoHz: 5e6, fHiHz: 6e6, t0: 1000 - 610, t1: 1000 + 10 });
});

// ---- bookmarks: form → NewBookmark ----

test("newBookmarkFromForm parses MHz/kHz and clips the name", () => {
  const b = newBookmarkFromForm({ name: "  My   Marker  ", freqMHz: "433.92", bandwidthKHz: "12.5", kind: "bookmark" });
  assert.deepEqual(b, { kind: "bookmark", name: "My Marker", f_center_hz: 433_920_000, bandwidth_hz: 12_500 });
});

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
