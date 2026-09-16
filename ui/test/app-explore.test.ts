// T-151 (ADR-0013 §4.2, §4.5, §8): Explore inventory sort/view-model, selections' found-inside and
// Listen-to-all, focus-panel actions against a fake client, and the slice's pure actions. No DOM
// under node:test: the "actions reachable without horizontal scroll" rule (T-148) is checked by
// reading explore.css as text, the technique ui/test/app-shell.test.ts uses.
// T-193 (docs/14-ui-rewrite.md "Added scope from docs/15 §7"): the user-band commit/reset actions
// (`PUT`/`DELETE /api/inventory/{id}/band`) and the optimistic single-row store patch.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { decodeActionLabel, emitterStreamAddress, recordEmitterClip, selectionSummary } from "../src/app/explore/focus";
import {
  clearUserBand, clusterChip, nextInventorySort, recurrenceDots, rowChips, rowSeenText, setUserBand,
  sortInventoryRows, type Classification, type Row,
} from "../src/app/explore/inventory";
import { foundInside, listenAllTargets, recordSelectionClip, type Selection } from "../src/app/explore/selections";
import {
  focusSelection, patchInventoryRow, removeInventoryRowLocal, restoreInventoryRowLocal, setInventoryError,
  setInventoryRows, setInventorySort, setInventoryTab, setSelections,
} from "../src/app/explore/slice";
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
    classification: null, explanations: [], refined: null, cluster_id: null,
    ...over,
  };
}

/** A full [[Classification]] fixture (T-207): the legacy fields plus every ADR-0016 field, so
 * tests can override just the ones they care about. */
function makeClassification(over: Partial<Classification> = {}): Classification {
  return {
    family: "wfm-broadcast", confidence: 0.9, open_set_score: 0.1, model_version: "v1", t_s: 0,
    taxonomy: "hk-mod@1", stage: "chain", arb_rank: 3, coarse: "analog",
    class: null, top: null, entropy_norm: null, flags: null,
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
    rowChips(makeRow({ family: null, classification: makeClassification() })),
    [{ cls: "known", text: "wfm-broadcast" }],
  );
  const flagged = makeRow({ explanations: [{ rank: 1, service: "fm-broadcast", label: "FM broadcast, off raster", score: 0.7, evidence_confidence: 0.7, status_evidence_confidence: 0, status: "known", prior_ref: null, flags: ["off-raster"], evidence: [] }] });
  assert.deepEqual(rowChips(flagged), [{ cls: "known", text: "wfm-broadcast" }, { cls: "flag", text: "off raster" }]);
});

test("clusterChip: 'seen before' when the row belongs to a visible cluster, else null", () => {
  assert.deepEqual(clusterChip(makeRow({ cluster_id: "cluster:0199abc" })), { cls: "cluster", text: "seen before" });
  assert.equal(clusterChip(makeRow({ cluster_id: null })), null);
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

// ---- user band (T-191 route, T-193 draggable box edges) ----

function fakeBandClient(handlers: { put?: (path: string, body?: unknown) => unknown; del?: (path: string) => unknown }) {
  return {
    put: async <T>(path: string, body?: unknown): Promise<T> => { if (!handlers.put) throw new Error(`unexpected PUT ${path}`); return handlers.put(path, body) as T; },
    del: async <T>(path: string): Promise<T> => { if (!handlers.del) throw new Error(`unexpected DELETE ${path}`); return handlers.del(path) as T; },
  };
}

test("setUserBand: PUTs the edges (id encoded), reports the server's updated entry", async () => {
  let seen: { path: string; body: unknown } | null = null;
  const entry = makeRow({ user_band: { f_lo: 1, f_hi: 2, set_at: 1, actor: "fp", reason: null, reason_withheld: false } });
  const client = fakeBandClient({ put: (path, body) => { seen = { path, body }; return { user_band: entry.user_band, entry }; } });
  const res = await setUserBand(client, "e/1", 1, 2);
  assert.deepEqual(res, { ok: true, entry });
  assert.equal(seen!.path, "/api/inventory/e%2F1/band");
  assert.deepEqual(seen!.body, { f_lo: 1, f_hi: 2 });
});

test("setUserBand: a refused band (e.g. too far from the measured extent) reports the server's own reason, not a thrown exception", async () => {
  const client = fakeBandClient({ put: () => { throw { code: "invalid", message: "band does not overlap the measured extent" }; } });
  const res = await setUserBand(client, "e1", 1, 2);
  assert.deepEqual(res, { ok: false, message: "band does not overlap the measured extent (invalid)" });
});

test("clearUserBand: DELETEs the override, reports the server's entry (measured band restored)", async () => {
  const entry = makeRow({ user_band: null });
  const client = fakeBandClient({ del: () => ({ cleared: true, entry }) });
  const res = await clearUserBand(client, "e1");
  assert.deepEqual(res, { ok: true, entry });
});

test("clearUserBand: a failure reports the server's reason", async () => {
  const client = fakeBandClient({ del: () => { throw { code: "not_found", message: "unknown id" }; } });
  const res = await clearUserBand(client, "gone");
  assert.deepEqual(res, { ok: false, message: "unknown id (not_found)" });
});

test("patchInventoryRow: merges a patch into one already-loaded row, leaving the rest untouched; a no-op for an unloaded id", () => {
  const s = createStore(initialState());
  const ub = { f_lo: 1, f_hi: 2, set_at: 1, actor: "fp", reason: null, reason_withheld: false };
  s.set(setInventoryRows({ e1: makeRow({ id: "e1" }), e2: makeRow({ id: "e2" }) }, 1));
  s.set(patchInventoryRow("e1", { user_band: ub }));
  assert.deepEqual(s.get().inventory.rows.e1.user_band, ub);
  assert.equal(s.get().inventory.rows.e2.user_band, undefined, "other rows untouched");
  const before = s.get().inventory.rows;
  s.set(patchInventoryRow("missing", { user_band: ub }));
  assert.equal(s.get().inventory.rows, before, "an unloaded row's patch is a no-op");
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

// ---- optimistic delete (T-187) ----

test("removeInventoryRowLocal drops one row without touching the rest; a no-op if it's already gone", () => {
  const s = createStore(initialState());
  const rows = { e1: makeRow({ id: "e1" }), e2: makeRow({ id: "e2" }) };
  s.set(setInventoryRows(rows, 1));
  s.set(removeInventoryRowLocal("e1"));
  assert.deepEqual(Object.keys(s.get().inventory.rows), ["e2"]);
  const before = s.get().inventory;
  s.set(removeInventoryRowLocal("e1"));
  assert.equal(s.get().inventory, before, "removing an absent row changes nothing");
});

test("restoreInventoryRowLocal puts a removed row back exactly as it was (delete-refused revert)", () => {
  const s = createStore(initialState());
  const row = makeRow({ id: "e1", state: "candidate" });
  s.set(setInventoryRows({ e1: row, e2: makeRow({ id: "e2" }) }, 1));
  s.set(removeInventoryRowLocal("e1"));
  assert.ok(!("e1" in s.get().inventory.rows));
  s.set(restoreInventoryRowLocal(row));
  assert.deepEqual(s.get().inventory.rows.e1, row);
  assert.ok("e2" in s.get().inventory.rows, "the other row is untouched");
});

test("deleting a confirmed row with a user-band override (T-193) drops its box; a refused delete restores the override intact", () => {
  // `live-spectrum.ts` derives the yellow boxes straight from `Object.values(inventory.rows)`
  // (confirmedBands), so removing the row is sufficient to drop its box, and restoring the exact
  // row object brings the override back rather than falling back to the measured extent.
  const s = createStore(initialState());
  const ub = { f_lo: 101_150_000, f_hi: 101_450_000, set_at: 1, actor: "fp", reason: null, reason_withheld: false };
  const row = makeRow({ id: "e1", state: "confirmed", user_band: ub });
  s.set(setInventoryRows({ e1: row }, 1));
  assert.deepEqual(s.get().inventory.rows.e1.user_band, ub);

  s.set(removeInventoryRowLocal("e1"));
  assert.ok(!("e1" in s.get().inventory.rows), "no stale row (and so no stale box) survives the optimistic delete");

  s.set(restoreInventoryRowLocal(row));
  assert.deepEqual(s.get().inventory.rows.e1.user_band, ub, "the band override comes back, not the measured extent");
});

// ---- layout: actions reachable without horizontal scroll (T-148) ----
// The focus panel's own action buttons (and their `.focus .actions` grid/400px rule) moved to the
// right-click/long-press context menu (T-192, ui/src/app/menu/); its viewport clamping is tested
// in app-menu.test.ts's `clampMenuPosition` tests instead. The sidebar row actions (Promote/
// Delete, still buttons in the list) keep the wrap rule this test originally checked.

test("explore.css: sidebar row actions wrap instead of overflowing", () => {
  const css = readFileSync("src/app/explore/explore.css", "utf8");
  assert.match(css, /\.side-inv \.acts\s*\{[^}]*flex-wrap:\s*wrap/);
  assert.doesNotMatch(css, /\.focus \.act\b/, "the focus panel's action buttons moved to the context menu (T-192)");
});

test("index.html: inventory and selections sit inside the left sidebar aside; the live view does not", () => {
  const html = readFileSync("src/app/index.html", "utf8");
  const start = html.indexOf('<aside class="side"');
  assert.ok(start >= 0, "no sidebar aside in src/app/index.html");
  const end = html.indexOf("</aside>", start);
  assert.ok(end > start);
  const region = html.slice(start, end);
  assert.match(region, /data-slot="inventory"/, "inventory slot not in the sidebar");
  assert.match(region, /data-slot="selections"/, "selections slot not in the sidebar");
  assert.doesNotMatch(region, /data-slot="live"/, "the live waterfall/spectrum must stay in the centre area");
});
