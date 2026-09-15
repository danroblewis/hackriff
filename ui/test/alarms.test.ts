// T-123: novelty alarm list panel (docs/api.md "Anomalies and novelty alarms"). No DOM under
// node:test: pure query/formatting/eligibility functions and the auth'd dismiss/reopen request
// shapes are tested directly (same technique as inventory.test.ts's promote/delete tests).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { ControlClient, type FetchFn } from "../src/controls/client";
import {
  type AnomalyRow, anomaliesQuery, canDismiss, canReopen, dismissAnomaly, freqRangeText,
  loadAnomalies, reopenAnomaly, topExplanationText,
} from "../src/alarms";

const html = readFileSync("src/index.html", "utf8");

// ---- query building ----

test("anomaliesQuery carries region, span, kind, status, cursor and limit", () => {
  const q = anomaliesQuery({ fLoHz: 1e6, fHiHz: 2e6, t0: 10, t1: 20, kind: "new-emitter", status: "open" }, "cur1", 50);
  const p = new URLSearchParams(q.split("?")[1]);
  assert.equal(p.get("f_lo"), "1000000");
  assert.equal(p.get("f_hi"), "2000000");
  assert.equal(p.get("t0"), "10");
  assert.equal(p.get("t1"), "20");
  assert.equal(p.get("kind"), "new-emitter");
  assert.equal(p.get("status"), "open");
  assert.equal(p.get("cursor"), "cur1");
  assert.equal(p.get("limit"), "50");
});

test("anomaliesQuery defaults limit to 100 and omits unset filters", () => {
  const q = anomaliesQuery({});
  const p = new URLSearchParams(q.split("?")[1]);
  assert.equal(p.get("limit"), "100");
  assert.equal(p.get("kind"), null);
  assert.equal(p.get("status"), null);
  assert.equal(p.get("cursor"), null);
});

// ---- explanation text: unexplained stays visible, never hidden ----

test("topExplanationText shows the top-ranked cause and its score", () => {
  const text = topExplanationText([{ id: "x", cause: { kind: "emitter" }, score: 0.83, provisional: false, t: 0 }]);
  assert.match(text, /emitter/);
  assert.match(text, /83%/);
});

test("topExplanationText shows unexplained plainly, like any other cause", () => {
  const text = topExplanationText([{ id: "x", cause: { kind: "unexplained" }, score: 0.4, provisional: false, t: 0 }]);
  assert.match(text, /unexplained/);
});

test("topExplanationText shows a self-inflicted reason when given", () => {
  const text = topExplanationText([{ id: "x", cause: { kind: "self-inflicted", reason: "gain change" }, score: 1, provisional: false, t: 0 }]);
  assert.match(text, /self-inflicted: gain change/);
});

test("topExplanationText is a dash with no explanations", () => {
  assert.equal(topExplanationText([]), "—");
});

test("freqRangeText formats Hz as MHz", () => {
  assert.equal(freqRangeText(433e6, 433.1e6), "433.0000–433.1000 MHz");
});

// ---- dismiss/reopen eligibility (docs/api.md: dismiss refused on a floor episode or self-inflicted) ----

function mkAnomaly(over: Partial<AnomalyRow> & { status: AnomalyRow["status"] }): AnomalyRow {
  return {
    id: "a1", kind: "new-emitter", subject: {}, f_lo: 1e6, f_hi: 2e6, t0: 0, t1: 1, t: 1, score: 0.9,
    alarm: { key: { kind: "new-emitter", site: {}, subject: {} }, state: "open", last_transition: "raised", raised_at: 0, last_t: 1, reopen_count: 0, f_lo: 1e6, f_hi: 2e6, detail: {} },
    explanations: [],
    ...over,
  };
}

test("canDismiss is true for an open alarm anomaly", () => {
  assert.equal(canDismiss(mkAnomaly({ status: "open" })), true);
});

test("canDismiss is false for a floor episode (no alarm)", () => {
  assert.equal(canDismiss(mkAnomaly({ status: "open", alarm: null })), false);
});

test("canDismiss is false for a self-inflicted (explained) anomaly", () => {
  assert.equal(canDismiss(mkAnomaly({ status: "resolved", alarm: { key: { kind: "new-emitter", site: {}, subject: {} }, state: "explained", last_transition: "explained", raised_at: 0, last_t: 1, reopen_count: 0, f_lo: 1e6, f_hi: 2e6, detail: {} } })), false);
});

test("canReopen is true only for a dismissed or cleared alarm", () => {
  const dismissed = mkAnomaly({ status: "dismissed", alarm: { key: { kind: "new-emitter", site: {}, subject: {} }, state: "dismissed", last_transition: "dismissed", raised_at: 0, last_t: 1, reopen_count: 0, f_lo: 1e6, f_hi: 2e6, detail: {} } });
  assert.equal(canReopen(dismissed), true);
  assert.equal(canReopen(mkAnomaly({ status: "open" })), false);
});

// ---- API calls: authenticated POST, request shapes ----

function fakeFetch(handler: (url: string, init: RequestInit) => { status: number; body: unknown }): { fn: FetchFn; seen: { url: string; init: RequestInit }[] } {
  const seen: { url: string; init: RequestInit }[] = [];
  const fn: FetchFn = async (url, init) => {
    seen.push({ url, init });
    const { status, body } = handler(url, init);
    return { ok: status >= 200 && status < 300, status, statusText: "", json: async () => body };
  };
  return { fn, seen };
}

test("dismissAnomaly sends an authenticated POST with the note and no query-string token", async () => {
  const { fn, seen } = fakeFetch(() => ({ status: 200, body: mkAnomaly({ status: "dismissed" }) }));
  const client = new ControlClient("tok-1", fn);
  await dismissAnomaly(client, "a 1", "false alarm");
  assert.equal(seen.length, 1);
  assert.equal(seen[0].url, "/api/anomalies/a%201/dismiss");
  assert.equal(seen[0].init.method, "POST");
  assert.equal((seen[0].init.headers as Record<string, string>).Authorization, "Bearer tok-1");
  assert.ok(!seen[0].url.includes("token="));
  assert.deepEqual(JSON.parse(seen[0].init.body as string), { note: "false alarm" });
});

test("dismissAnomaly with no note sends an empty body object", async () => {
  const { fn, seen } = fakeFetch(() => ({ status: 200, body: mkAnomaly({ status: "dismissed" }) }));
  const client = new ControlClient("tok-1", fn);
  await dismissAnomaly(client, "a1");
  assert.deepEqual(JSON.parse(seen[0].init.body as string), {});
});

test("reopenAnomaly sends an authenticated POST", async () => {
  const { fn, seen } = fakeFetch(() => ({ status: 200, body: mkAnomaly({ status: "open" }) }));
  const client = new ControlClient("tok-2", fn);
  await reopenAnomaly(client, "a1");
  assert.equal(seen[0].url, "/api/anomalies/a1/reopen");
  assert.equal(seen[0].init.method, "POST");
});

test("loadAnomalies GETs the built query and returns the page", async () => {
  const calls: string[] = [];
  const client = { get: async (path: string) => { calls.push(path); return { anomalies: [mkAnomaly({ status: "open" })], next_cursor: null, truncated: false, suppressions: {} }; }, post: async () => { throw new Error("not used"); } };
  const page = await loadAnomalies(client, { status: "open" });
  assert.equal(page.anomalies.length, 1);
  assert.ok(calls[0].includes("status=open"));
});

// ---- layout ----

test("index.html has the alarm list panel with filters, table and detail/dismiss/reopen affordances", () => {
  assert.ok(html.includes('id="alarms"'), "no #alarms panel");
  assert.ok(html.includes('id="al-kind"') && html.includes('id="al-status"'), "kind/status filters missing");
  assert.ok(html.includes('id="al-body"'), "no alarm table body");
  assert.ok(html.includes('id="al-detail"'), "no detail panel");
});
