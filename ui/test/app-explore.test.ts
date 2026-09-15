// T-151 (ADR-0013 §4.2, §4.5, §8): Explore inventory sort/view-model, selections' found-inside and
// Listen-to-all, focus-panel actions against a fake client, and the slice's pure actions. No DOM
// under node:test: the "actions reachable without horizontal scroll" rule (T-148) is checked by
// reading explore.css as text, the technique ui/test/app-shell.test.ts uses.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { decodeActionLabel, emitterStreamAddress, recordEmitterClip, selectionSummary } from "../src/app/explore/focus";
import {
  nextInventorySort, recurrenceDots, rowChips, rowSeenText, sortInventoryRows, type Row,
} from "../src/app/explore/inventory";
import { foundInside, listenAllTargets, recordSelectionClip, type Selection } from "../src/app/explore/selections";
import { focusSelection, setInventoryError, setInventoryRows, setInventorySort, setInventoryTab, setSelections } from "../src/app/explore/slice";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";

// ---- fixtures ----

function makeRow(over: Partial<Row> = {}): Row {
  return {
    id: "e1", state: "confirmed",
    f_center_hz: 101_300_000, bandwidth_hz: 183_000, f_lo_hz: 101_208_500, f_hi_hz: 101_391_500,
    first_seen_s: 1_789_300_000, last_seen_s: 1_789_300_900, count: 42,
    known_status: "known", status: null, tags: [], family: "wfm-broadcast",
    identity_scheme: null, identity_class: null, withheld: false,
    recurrence: { occurrences: 12, appearances: 3, span_s: 3600, on_air_s: 900, duty_cycle: 0.25, recent: [] },
    classification: null, explanations: [], refined: null,
    ...over,
  };
}

function makeSelection(over: Partial<Selection> = {}): Selection {
  return { id: "s1", name: "Around 101.3", f_lo: 101_180_000, f_hi: 101_620_000, tags: [], links: [], created: 1, updated: 1, ...over };
}

// ---- sort ----

test("sortInventoryRows: freq, last_seen, count, bandwidth, both directions", () => {
  const rows = [
    makeRow({ id: "a", f_center_hz: 101_300_000, last_seen_s: 100, count: 5, bandwidth_hz: 150_000 }),
    makeRow({ id: "b", f_center_hz: 99_700_000, last_seen_s: 300, count: 1, bandwidth_hz: 12_000 }),
    makeRow({ id: "c", f_center_hz: 100_300_000, last_seen_s: 200, count: 42, bandwidth_hz: 183_000 }),
  ];
  assert.deepEqual(sortInventoryRows(rows, "freq", 1).map((r) => r.id), ["b", "c", "a"]);
  assert.deepEqual(sortInventoryRows(rows, "freq", -1).map((r) => r.id), ["a", "c", "b"]);
  assert.deepEqual(sortInventoryRows(rows, "last_seen", 1).map((r) => r.id), ["a", "c", "b"]);
  assert.deepEqual(sortInventoryRows(rows, "count", -1).map((r) => r.id), ["c", "a", "b"]);
  assert.deepEqual(sortInventoryRows(rows, "bandwidth", 1).map((r) => r.id), ["b", "a", "c"]);
  // original array is untouched (pure)
  assert.equal(rows[0].id, "a");
});

test("nextInventorySort: toggles the active column, switches to a new one at its default direction", () => {
  const start = { key: "freq" as const, dir: 1 as const };
  assert.deepEqual(nextInventorySort(start, "freq"), { key: "freq", dir: -1 });
  assert.deepEqual(nextInventorySort(start, "count"), { key: "count", dir: -1 }); // busiest first by default
  assert.deepEqual(nextInventorySort(start, "last_seen"), { key: "last_seen", dir: -1 }); // most recent first
  assert.deepEqual(nextInventorySort(start, "bandwidth"), { key: "bandwidth", dir: 1 });
});

// ---- row view model ----

test("rowChips: known family, unknown family, off-raster flag", () => {
  assert.deepEqual(rowChips(makeRow({ family: "wfm-broadcast" })), [{ cls: "known", text: "wfm-broadcast" }]);
  assert.deepEqual(rowChips(makeRow({ family: null, classification: null })), [{ cls: "unknown", text: "unknown" }]);
  assert.deepEqual(
    rowChips(makeRow({ family: null, classification: { family: "wfm-broadcast", confidence: 0.9, open_set_score: 0.1, model_version: "v1", t_s: 0 } })),
    [{ cls: "known", text: "wfm-broadcast" }],
  );
  const flagged = makeRow({ explanations: [{ rank: 1, service: "fm-broadcast", label: "FM broadcast, off raster", score: 0.7, evidence_confidence: 0.7, status_evidence_confidence: 0, status: "known", prior_ref: null, flags: ["off-raster"], evidence: [] }] });
  assert.deepEqual(rowChips(flagged), [{ cls: "known", text: "wfm-broadcast" }, { cls: "flag", text: "off raster" }]);
});

test("rowSeenText: confirmed shows on-air duty and count (GAP 2 interim); candidates show a rate", () => {
  assert.equal(rowSeenText(makeRow({ state: "confirmed", count: 42, recurrence: { occurrences: 1, appearances: 1, span_s: 1, on_air_s: 1, duty_cycle: 1, recent: [] } })), "100% on-air · 42 seen");
  assert.equal(
    rowSeenText(makeRow({ state: "candidate", recurrence: { occurrences: 14, appearances: 4, span_s: 3600, on_air_s: 300, duty_cycle: 0.08, recent: [] } })),
    "14×/h",
  );
  assert.equal(
    rowSeenText(makeRow({ state: "candidate", recurrence: { occurrences: 1, appearances: 1, span_s: 5400, on_air_s: 30, duty_cycle: 0.01, recent: [] } })),
    "1× in 90 min",
  );
  assert.equal(rowSeenText(makeRow({ recurrence: null, count: 7 })), "7 seen");
});

test("recurrenceDots: normalises recent counts 0-1, most recent last, empty without history", () => {
  const rec = { occurrences: 3, appearances: 3, span_s: 100, on_air_s: 10, duty_cycle: 0.1, recent: [
    { t_start_s: 0, t_end_s: 1, count: 2, duty_cycle: 1 },
    { t_start_s: 1, t_end_s: 2, count: 4, duty_cycle: 1 },
    { t_start_s: 2, t_end_s: 3, count: 1, duty_cycle: 1 },
  ] };
  assert.deepEqual(recurrenceDots(makeRow({ recurrence: rec })), [0.5, 1, 0.25]);
  assert.deepEqual(recurrenceDots(makeRow({ recurrence: null })), []);
  assert.deepEqual(recurrenceDots(makeRow({ recurrence: { ...rec, recent: [] } })), []);
});

// ---- selections ----

test("foundInside: filters by overlap, busiest first then most recently seen", () => {
  const sel = makeSelection({ f_lo: 101_000_000, f_hi: 101_500_000 });
  const inside1 = makeRow({ id: "in-busy", f_lo_hz: 101_100_000, f_hi_hz: 101_200_000, count: 40, last_seen_s: 100 });
  const inside2 = makeRow({ id: "in-recent", f_lo_hz: 101_300_000, f_hi_hz: 101_400_000, count: 40, last_seen_s: 200 });
  const outside = makeRow({ id: "out", f_lo_hz: 200_000_000, f_hi_hz: 200_100_000 });
  assert.deepEqual(foundInside(sel, [inside1, outside, inside2]).map((r) => r.id), ["in-recent", "in-busy"]);
});

test("listenAllTargets: one emitter target per row", () => {
  const targets = listenAllTargets([makeRow({ id: "e1", f_center_hz: 101_300_000 })]);
  assert.deepEqual(targets, [{ kind: "emitter", emitterId: "e1", label: "101.3000 MHz" }]);
});

test("selectionSummary: extent width plus the found count", () => {
  assert.equal(selectionSummary(makeSelection({ f_lo: 101_180_000, f_hi: 101_620_000 }), 3), "440 kHz wide · 3 signals inside");
  assert.equal(selectionSummary(makeSelection({ f_lo: 0, f_hi: 2_000_000 }), 1), "2.00 MHz wide · 1 signal inside");
});

// ---- focus panel actions, against a fake client ----

function fakeClient(handlers: { get?: (path: string) => unknown; post?: (path: string, body?: unknown) => unknown }) {
  return {
    get: async <T>(path: string): Promise<T> => {
      if (!handlers.get) throw new Error(`unexpected GET ${path}`);
      return handlers.get(path) as T;
    },
    post: async <T>(path: string, body?: unknown): Promise<T> => {
      if (!handlers.post) throw new Error(`unexpected POST ${path}`);
      return handlers.post(path, body) as T;
    },
  };
}

test("decodeActionLabel: names the known scheme, else the generic invitation", () => {
  assert.equal(decodeActionLabel({ identity_scheme: "rds-pi" }), "Decode RDS");
  assert.equal(decodeActionLabel({ identity_scheme: null }), "Open in Decode");
});

test("recordEmitterClip: posts emitter_id and the record kinds, reports the server's kinds", async () => {
  let seen: { path: string; body: unknown } | null = null;
  const client = fakeClient({ post: (path, body) => { seen = { path, body }; return { recording: { kinds: ["iq", "bits", "symbols", "audio"] } }; } });
  const res = await recordEmitterClip(client, "e1");
  assert.deepEqual(res, { ok: true, kinds: ["iq", "bits", "symbols", "audio"] });
  assert.equal(seen!.path, "/api/outputs/record/start");
  assert.deepEqual(seen!.body, { emitter_id: "e1", kinds: ["iq", "bits", "symbols", "audio"] });
});

test("recordEmitterClip: a refused request reports the server's error text, not a thrown exception", async () => {
  const client = fakeClient({ post: () => { throw { code: "quota", message: "output quota full" }; } });
  const res = await recordEmitterClip(client, "e1");
  assert.deepEqual(res, { ok: false, message: "output quota full (quota)" });
});

test("recordSelectionClip: posts the selection's band extent", async () => {
  let seen: unknown = null;
  const client = fakeClient({ post: (_p, body) => { seen = body; return { recording: { kinds: ["iq"] } }; } });
  const res = await recordSelectionClip(client, makeSelection({ f_lo: 1, f_hi: 2 }));
  assert.deepEqual(res, { ok: true, kinds: ["iq"] });
  assert.deepEqual(seen, { band: { f_lo: 1, f_hi: 2 }, kinds: ["iq", "bits", "symbols", "audio"] });
});

test("emitterStreamAddress: the tcp address plus the listen opener's target, no token", async () => {
  const client = fakeClient({ get: () => ({ tcp: { addr: "127.0.0.1:8788" }, on_demand: [{ name: "listen", tcp_target: "open/listen" }] }) });
  assert.equal(await emitterStreamAddress(client, "e1"), "tcp://127.0.0.1:8788 open/listen?emitter=e1");
});

test("emitterStreamAddress: null when the server offers no tcp server or listen opener", async () => {
  const client = fakeClient({ get: () => ({ tcp: null, on_demand: [] }) });
  assert.equal(await emitterStreamAddress(client, "e1"), null);
});

// ---- slice actions ----

test("explore slice actions: tab, sort, rows, error, selections, focus", () => {
  const s = createStore(initialState());
  s.set(setInventoryTab("candidate"));
  assert.equal(s.get().inventory.tab, "candidate");
  s.set(setInventorySort({ key: "count", dir: -1 }));
  assert.deepEqual(s.get().inventory.sort, { key: "count", dir: -1 });
  const rows = { e1: makeRow() };
  s.set(setInventoryRows(rows, 123));
  assert.deepEqual(s.get().inventory.rows, rows);
  assert.equal(s.get().inventory.loadedAtS, 123);
  s.set(setInventoryError("boom"));
  assert.equal(s.get().inventory.error, "boom");
  // a fresh load clears a stale error
  s.set(setInventoryRows(rows, 456));
  assert.equal(s.get().inventory.error, null);
  const sels = [makeSelection()];
  s.set(setSelections(sels, "1 region · saved on the server"));
  assert.deepEqual(s.get().selections, { list: sels, sync: "1 region · saved on the server" });
  s.set(focusSelection("s1"));
  assert.deepEqual(s.get().focus, { kind: "selection", id: "s1" });
});

// ---- layout: actions reachable without horizontal scroll (T-148) ----

test("explore.css: row and focus actions wrap instead of overflowing", () => {
  const css = readFileSync("src/app/explore/explore.css", "utf8");
  assert.match(css, /\.side-inv \.acts\s*\{[^}]*flex-wrap:\s*wrap/);
  assert.match(css, /\.focus \.actions\s*\{[^}]*grid-template-columns/);
  assert.match(css, /@media \(max-width:\s*400px\)\s*\{\s*\.focus \.actions\s*\{[^}]*grid-template-columns:\s*1fr/);
});
