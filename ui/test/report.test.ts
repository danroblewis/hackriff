// T-123: survey report panel (docs/api.md "Survey reports"). No DOM under node:test (same
// technique as inventory.test.ts/frame-inspector.test.ts): pure query/formatting functions and the
// thin API wrapper are tested directly; index.html is read as text for the layout checks.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  changeStatusText, fcoText, fmtNs, freqText, loadReport, reportExportUrl, reportQuery,
  type ReportClient, type SurveyReport,
} from "../src/report";

const html = readFileSync("src/index.html", "utf8");

// ---- query building ----

test("reportQuery carries region, span and an optional site", () => {
  const q = reportQuery({ fLoHz: 100e6, fHiHz: 102e6, t0: 1789300800, t1: 1789300860, site: "site-1" });
  const p = new URLSearchParams(q.split("?")[1]);
  assert.equal(p.get("f_lo"), "100000000");
  assert.equal(p.get("f_hi"), "102000000");
  assert.equal(p.get("t0"), "1789300800");
  assert.equal(p.get("t1"), "1789300860");
  assert.equal(p.get("site"), "site-1");
});

test("reportQuery omits site when not given", () => {
  const q = reportQuery({ fLoHz: 1e6, fHiHz: 2e6, t0: 0, t1: 1 });
  assert.ok(!q.includes("site="), q);
});

test("reportExportUrl adds format=csv|png to the same query", () => {
  const p = { fLoHz: 1e6, fHiHz: 2e6, t0: 0, t1: 1 };
  assert.ok(reportExportUrl(p, "csv").endsWith("&format=csv"));
  assert.ok(reportExportUrl(p, "png").endsWith("&format=png"));
  assert.ok(reportExportUrl(p, "csv").startsWith(reportQuery(p)));
});

// ---- fco: fco vs fco_all_visits (null-fco "indicative (biased)" label) ----

test("fcoText shows the unbiased fco figure when present", () => {
  assert.equal(fcoText({ fco: 0.123, fco_all_visits: 0.4 }), "12.3%");
});

test("fcoText labels fco_all_visits as indicative/biased when fco is null (history-tile stand-in, T-121)", () => {
  assert.equal(fcoText({ fco: null, fco_all_visits: 0.314 }), "31.4% (indicative, biased)");
});

test("fcoText is a dash when neither figure is available", () => {
  assert.equal(fcoText({ fco: null, fco_all_visits: null }), "—");
});

test("freqText and fmtNs format Hz ranges and nanosecond timestamps", () => {
  assert.equal(freqText({ lo_hz: 100e6, hi_hz: 102e6 }), "100.0000–102.0000 MHz");
  assert.equal(freqText(undefined), "—");
  assert.equal(fmtNs(undefined), "—");
  assert.ok(fmtNs(1_789_300_800_000_000_000).startsWith("2026-"), fmtNs(1_789_300_800_000_000_000));
});

test("changeStatusText names every change_vs_baseline.status value", () => {
  assert.match(changeStatusText("available"), /available/);
  assert.match(changeStatusText("immature"), /immature/i);
  assert.match(changeStatusText("no-baseline"), /no baseline/i);
  assert.match(changeStatusText("unavailable"), /unavailable/i);
});

// ---- loadReport: the thin GET wrapper ----

function mockReport(): SurveyReport {
  return {
    schema: 1, generated_at: 1_789_300_860_000_000_000,
    region: { lo_hz: 100e6, hi_hz: 102e6 }, span: { start: 1_789_300_800_000_000_000, end: 1_789_300_860_000_000_000 },
    site: { kind: "unassigned" },
    occupancy: {
      bands: [{ subject: { kind: "band" }, fco: null, fco_all_visits: 0.31, fbo: 0.08, n_revisits: 0, n_revisits_all: 120, observed_s: 61.5, revisit_biased: true, subject_extent: { lo_hz: 100e6, hi_hz: 102e6 } }],
      channels: [], truncated: false,
    },
    top_emitters: [{ emitter_id: "e1", freq: { lo_hz: 100.9e6, hi_hz: 101.0e6 }, first_seen: 0, last_seen: 1, sightings: 3, lifecycle: "candidate", fco: null, fco_all_visits: 0.5, top_suggestion: "FM broadcast", new_in_span: true }],
    change_vs_baseline: { status: "unavailable", changes: [] },
    coverage: { observed_fraction: 0.42, observed_s: 61.5, gaps: [], gaps_truncated: false, never_observed: [], poi: [{ tau_s: 0.1, p_poi: 0.2 }], statement: "unobserved is not quiet" },
    provenance_steps: [{ t: 0, kind: "gain", detail: "lna 32→24 dB" }],
    anomalies: [], warnings: ["history-tile occupancy: fco is unavailable"],
  };
}

test("loadReport GETs the report query and returns the parsed document", async () => {
  const calls: string[] = [];
  const client: ReportClient = { get: async (path) => { calls.push(path); return mockReport(); } };
  const r = await loadReport(client, { fLoHz: 100e6, fHiHz: 102e6, t0: 1789300800, t1: 1789300860 });
  assert.equal(calls.length, 1);
  assert.ok(calls[0].startsWith("/api/report?"));
  assert.equal(r.coverage.statement, "unobserved is not quiet");
  assert.equal(r.occupancy.bands[0].fco, null);
  assert.equal(r.top_emitters[0].top_suggestion, "FM broadcast");
});

// ---- layout: the panel exists and the coverage/export affordances are present ----

test("index.html has the survey report panel with coverage, export links and use-region button", () => {
  assert.ok(html.includes('id="report"'), "no #report panel");
  assert.ok(html.includes('id="rpt-form"'));
  assert.ok(html.includes('id="rpt-coverage-statement"'), "coverage statement element missing");
  assert.ok(html.includes('id="rpt-csv"') && html.includes('id="rpt-png"'), "CSV/PNG export links missing");
  assert.ok(html.includes('id="rpt-use-region"'), "no button to prefill from the current region");
});
