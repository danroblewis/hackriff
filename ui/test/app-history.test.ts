// T-264 (ADR-0017 stage TM-8): the History surface's pure helpers — region/period defaults, the
// query builders, timespan wording and the empty-state rule. No DOM under node:test, so the
// DOM-wiring class (CatalogueSurface) is exercised manually against a running `hk serve`, like
// every other MUI panel.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  DEFAULT_SPAN_S, INDEPENDENT_PERIOD_NOTE, defaultRegion, defaultRegionNote, emptyText,
  eventRowView, eventsQuery, lastedText, summaryText, trackQuery,
  type CatalogueEmitter, type CatalogueEvent, type CataloguePage,
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
  assert.deepEqual(r, { kind: "region", region: { fLoHz: 100e6, fHiHz: 102e6, t0: 1000 - DEFAULT_SPAN_S, t1: 1000 } });
});

test("defaultRegion falls back to the tuned band, and opens on nothing with nothing tuned", () => {
  const r = defaultRegion({ live: null, device: { centerHz: 101e6, sampleRateHz: 2e6 } }, 5);
  assert.equal(r.kind === "region" && r.region.fLoHz, 100e6);
  assert.equal(r.kind === "region" && r.region.fHiHz, 102e6);
  assert.deepEqual(defaultRegion({ live: null, device: { centerHz: null, sampleRateHz: null } }, 5), { kind: "no-band" });
});

// ---- T-386: the period opens on the CAPTURE clock, and the surface says which window it answers
// about ----------------------------------------------------------------------------------------

/** The capture clock these fixtures run on, deliberately far from any wall clock (T-379). */
const CAPTURE_EDGE_S = 1_789_297_847;

test("T-386 THE PROPERTY: the period the catalogue opens on ends at the capture clock's live edge", () => {
  // The sixth site of the T-379 bug. This surface opened on `Date.now() / 1000`, so on a capture
  // running 3.5 days from wall time it opened on a day the receiver was never switched on for —
  // and then showed the user an empty catalogue as the surface's first impression.
  const d = defaultRegion({ live: { loHz: 100e6, hiHz: 102e6 }, device: { centerHz: null, sampleRateHz: null } }, CAPTURE_EDGE_S);
  assert.equal(d.kind, "region");
  assert.equal(d.kind === "region" && d.region.t1, CAPTURE_EDGE_S);
  assert.equal(d.kind === "region" && d.region.t0, CAPTURE_EDGE_S - DEFAULT_SPAN_S);
  // The property, not the value: whatever clock the capture runs on, the catalogue opens on THAT.
  assert.ok(d.kind === "region" && Math.abs(d.region.t1 - Date.now() / 1000) > 1000, "the capture clock, never Date.now()");
});

test("T-386 THE CONTROL: with no capture clock reported the surface opens on NO period rather than a plausible one", () => {
  // Without this the property is satisfiable by always producing *some* period, which is exactly
  // how the bug survived five earlier findings: a window was always produced, on the wrong clock.
  const d = defaultRegion({ live: { loHz: 100e6, hiHz: 102e6 }, device: { centerHz: null, sampleRateHz: null } }, null);
  assert.deepEqual(d, { kind: "no-clock" });
  // And the two missing answers say different things: no band tuned is not no clock reported.
  assert.notEqual(defaultRegionNote({ kind: "no-clock" }), defaultRegionNote({ kind: "no-band" }));
  assert.match(defaultRegionNote({ kind: "no-clock" }), /capture clock/i);
});

test("T-386: the History surface states, standingly, that it is NOT the view window", () => {
  // The decision: History stays independent of the cursor (CLAUDE.md puts the durable all-time
  // record in a separate surface, and workflow #3 is "choose a region"). T-387's obligation then
  // applies instead of T-379's: a surface answering about its own window must say which one.
  assert.match(INDEPENDENT_PERIOD_NOTE, /not the window the waterfall is showing/);
  const src = readFileSync("src/app/history/index.ts", "utf8");
  // Standing, not conditional: it is put in the DOM once, never set from a render branch.
  assert.match(src, /this\.scope\b/, "the note has its own element");
  assert.ok(!/scope\.textContent\s*=/.test(src), "the scope note must never be swapped out by a render path");
  assert.match(src, /this\.scope, this\.note/, "and it sits above the answer it qualifies");
  // The capture clock, and not a one-shot: opening the surface before the first `/api/timeline`
  // answer must not leave it permanently on "no period reported".
  assert.match(src, /defaultRegion\(\{ live: s\.live\.view, device: s\.device \}, liveEdgeS\(s\)\)/);
  assert.match(src, /if \(d\.kind !== "region"\) \{[^}]*return; \}\s*\n\s*this\.loaded = true;/, "`loaded` latches only on success");
  assert.match(src, /store\.select\(\(s\) => `\$\{liveEdgeS\(s\)\}/, "and it retries when a clock is first reported");
});

test("T-386 CLOCK GUARD: no clock of the browser's own reaches the History modules", () => {
  // The structural half of the capture-clock property (T-393's guard, extended to this surface):
  // `defaultRegion` cannot start reading wall time again in a later edit without this failing.
  // ISO formatting of a *served* absolute time is not a clock and is deliberately not listed.
  for (const f of ["src/app/history/catalogue.ts", "src/app/history/index.ts", "src/history.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset"]) {
      assert.ok(!src.includes(word), `${f} must not contain "${word}"`);
    }
  }
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
