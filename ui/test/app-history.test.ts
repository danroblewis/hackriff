// T-264 (ADR-0017 stage TM-8): the History surface's pure helpers — region/period defaults, the
// query builders, timespan wording and the empty-state rule. No DOM under node:test, so the
// DOM-wiring class (CatalogueSurface) is exercised manually against a running `hk serve`, like
// every other MUI panel.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  DEFAULT_SPAN_S, defaultRegion, emptyText, eventRowView, eventsQuery, lastedText, summaryText,
  trackQuery, type CatalogueEmitter, type CatalogueEvent, type CataloguePage,
} from "../src/app/history/catalogue";

const emitter = (over: Partial<CatalogueEmitter> = {}): CatalogueEmitter => ({
  id: "e1", state: "candidate", f_center_hz: 915.2e6, bandwidth_hz: 120e3,
  f_lo_hz: 915.14e6, f_hi_hz: 915.26e6, known_status: "unknown", family: null,
  explanations: [], identity_scheme: null, withheld: false, events: 1, on_air_s: 0.04, count: 1,
  ...over,
});

const event = (over: Partial<CatalogueEvent> = {}): CatalogueEvent => ({
  emitter_id: "e1", t_start_s: 1789300871, t_end_s: 1789300871.04, duration_s: 0.04,
  in_window_s: 0.04, open: false, count: 1, sources: 1, f_center_hz: 915.2e6, ...over,
});

const page = (over: Partial<CataloguePage> = {}): CataloguePage => ({
  window: { f_lo_hz: 915e6, f_hi_hz: 916e6, t0_s: 0, t1_s: 10 },
  events: [], emitters: [], total: 0, limit: 200, next_cursor: null,
  emitters_truncated: false, emitters_no_interval: 0,
  coverage: {
    source: "spectrum-history", observed_fraction: 1, cells: 10, observed_cells: 10,
    gaps: [], gaps_truncated: false,
    statement: "This region was observed throughout this period, so an empty catalogue means nothing was on the air here.",
  },
  ...over,
});

// ---- region and period defaults ----

test("defaultRegion opens on the live view over the last day", () => {
  const r = defaultRegion({ live: { loHz: 100e6, hiHz: 102e6 }, device: { centerHz: null, sampleRateHz: null } }, 1000);
  assert.deepEqual(r, { fLoHz: 100e6, fHiHz: 102e6, t0: 1000 - DEFAULT_SPAN_S, t1: 1000 });
});

test("defaultRegion falls back to the tuned band, and is null with nothing tuned", () => {
  const r = defaultRegion({ live: null, device: { centerHz: 101e6, sampleRateHz: 2e6 } }, 5);
  assert.equal(r?.fLoHz, 100e6);
  assert.equal(r?.fHiHz, 102e6);
  assert.equal(defaultRegion({ live: null, device: { centerHz: null, sampleRateHz: null } }, 5), null);
});

// ---- queries ----

test("eventsQuery always carries the box, and omits an empty state filter and cursor", () => {
  const q = eventsQuery({ fLoHz: 1e6, fHiHz: 2e6, t0: 10, t1: 20 }, "");
  assert.match(q, /^\/api\/events\?/);
  for (const p of ["f_lo=1000000", "f_hi=2000000", "t0=10", "t1=20", "limit=200"]) assert.ok(q.includes(p), `${q} has ${p}`);
  assert.ok(!q.includes("state="), q);
  assert.ok(!q.includes("cursor="), q);
  const q2 = eventsQuery({ fLoHz: 1e6, fHiHz: 2e6, t0: 10, t1: 20 }, "confirmed", "200");
  assert.ok(q2.includes("state=confirmed") && q2.includes("cursor=200"), q2);
});

test("trackQuery asks for one emitter's track over the same period", () => {
  assert.equal(trackQuery("e1", { fLoHz: 1, fHiHz: 2, t0: 10, t1: 20 }), "/api/inventory/e1/presence?t0=10&t1=20");
});

// ---- a one-off burst is an event with its measured timespan ----

test("lastedText keeps a sub-second burst in milliseconds instead of rounding it to nothing", () => {
  assert.equal(lastedText(0.04), "40 ms");
  assert.equal(lastedText(0), "0 ms");
  assert.equal(lastedText(12.5), "12.5 s");
  assert.equal(lastedText(90), "1.5 min");
  assert.equal(lastedText(7200), "2.0 h");
  assert.equal(lastedText(Number.NaN), "—");
});

test("eventRowView reports the served duration, never one re-derived from the timestamps", () => {
  // A row whose duration_s disagrees with t_end − t_start must still show the served figure: the
  // backend computes the timespan (thin-client rule), and a UI that recomputed it would silently
  // disagree with the catalogue and the scrubber.
  const v = eventRowView(event({ duration_s: 0.04, t_start_s: 0, t_end_s: 99 }), emitter());
  assert.equal(v.lasted, "40 ms");
});

test("eventRowView shows a ranked explanation as a suggestion, never as what the signal is", () => {
  const v = eventRowView(event(), emitter({ explanations: [{ rank: 1, service: "band-plan", label: "ISM 902–928 MHz", score: 0.4, flags: [] }] }));
  assert.equal(v.what, "ISM 902–928 MHz?");
  assert.equal(eventRowView(event(), emitter()).what, "unknown");
  assert.equal(eventRowView(event(), emitter({ family: "wfm-broadcast" })).what, "wfm-broadcast");
  assert.equal(eventRowView(event(), emitter({ identity_value: "A1B2" })).what, "A1B2");
});

test("eventRowView carries the backend's `open`, and an event with no emitter still renders", () => {
  assert.equal(eventRowView(event({ open: true }), emitter()).open, true);
  const orphan = eventRowView(event({ f_center_hz: null }), undefined);
  assert.equal(orphan.freq, "—");
  assert.equal(orphan.what, "unknown");
});

// ---- no coverage is never "nothing was on air" ----

test("an empty catalogue shows the backend's coverage statement, never a verdict of its own", () => {
  const unobserved = page({ coverage: { ...page().coverage, observed_cells: 0, observed_fraction: 0, statement: "Nothing here was observed in this period: no data for this period, not a quiet band." } });
  assert.equal(emptyText(unobserved, null), "Nothing here was observed in this period: no data for this period, not a quiet band.");
  assert.match(emptyText(page(), null), /nothing was on the air/);
  const unknown = page({ coverage: { ...page().coverage, source: null, observed_fraction: null, cells: null, observed_cells: null, gaps: null, gaps_truncated: null, statement: "Coverage is unknown: this server keeps no spectrum history…" } });
  assert.match(emptyText(unknown, null), /unknown/);
  assert.doesNotMatch(emptyText(unknown, null), /nothing was on the air/);
});

test("an error and an unasked question are both distinct from an empty answer", () => {
  assert.equal(emptyText(null, "unauthorized (401)"), "unauthorized (401)");
  assert.equal(emptyText(null, null), "Choose a region and a period.");
  assert.equal(emptyText(page({ total: 1, events: [event()] }), null), "");
});

// ---- what the answer left out is disclosed, never silent ----

test("summaryText counts the answer and discloses truncation and interval-less rows", () => {
  assert.equal(summaryText(page({ total: 1, events: [event()], emitters: [emitter()] })), "1 event · 1 emitter");
  const cut = summaryText(page({ total: 9, events: [event(), event()], emitters: [emitter()], emitters_truncated: true, emitters_no_interval: 3 }));
  assert.match(cut, /9 events/);
  assert.match(cut, /showing 2/);
  assert.match(cut, /more emitters matched/);
  assert.match(cut, /3 row\(s\) here carry no presence interval/);
});
