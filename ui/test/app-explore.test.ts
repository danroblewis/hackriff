// T-151 (ADR-0013 §4.2, §4.5, §8): Explore inventory sort/view-model, selections' found-inside and
// Listen-to-all, focus-panel actions against a fake client, and the slice's pure actions. No DOM
// under node:test: the "actions reachable without horizontal scroll" rule (T-148) is checked by
// reading explore.css as text, the technique ui/test/app-shell.test.ts uses.
// T-193 (docs/14-ui-rewrite.md "Added scope from docs/15 §7"): the user-band commit/reset actions
// (`PUT`/`DELETE /api/inventory/{id}/band`) and the optimistic single-row store patch.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { AppContext } from "../src/app/context";
import {
  decodeActionLabel, emitterStreamAddress, fetchEmitterLookup, recordEmitterClip, selectionSummary,
  signalFocus, signalFocusText,
} from "../src/app/explore/focus";
import {
  clearUserBand, CLUSTER_CHIP_TITLE, clusterChip, confirmedFilters, DEFAULT_ROW_RATE_HZ, emptyListText,
  explanationChip, explanationReasonText, explanationState, liveEdgeS,
  loadInventoryRows, nextInventorySort, recurrenceDots, renderedInventory, rowChips, rowSeenText,
  setUserBand, sortInventoryRows, viewFilters, viewWindow, WAITING_FOR_WINDOW, waterfallSpanS,
  type Classification, type Row, FALLBACK_ROWS,
} from "../src/app/explore/inventory";
import { ARTIFACT_MARK, CANDIDATE_MARK, CONFIRMED_MARK, signalMarkBoxes } from "../src/surface/marks";
import * as ax from "../src/axis";

import {
  foundInside, listenAllTargets, recordSelectionClip, selectionsEmptyText, selectionsInWindow,
  selectionStoreFor, type Selection,
} from "../src/app/explore/selections";
import { commitRegion, regionDestination, regionIsReal } from "../src/app/explore/region";
import {
  focusSelection, patchInventoryRow, removeInventoryRowLocal, restoreInventoryRowLocal, setBandEdit,
  setInventoryError, setInventoryRows, setInventorySort, setInventoryTab, setSelections,
} from "../src/app/explore/slice";
import { createStore } from "../src/app/store";
import { initialState, reviewAt } from "../src/app/state";

// ---- fixtures ----

function makeRow(over: Partial<Row> = {}): Row {
  return {
    id: "e1", state: "confirmed",
    f_center_hz: 101_300_000, bandwidth_hz: 183_000, f_lo_hz: 101_208_500, f_hi_hz: 101_391_500,
    first_seen_s: 1_789_300_000, last_seen_s: 1_789_300_900, count: 42,
    known_status: "known", status: null, tags: [], family: "wfm-broadcast",
    identity_scheme: null, identity_class: null, withheld: false,
    recurrence: { occurrences: 12, appearances: 3, span_s: 3600, on_air_s: 900, duty_cycle: 0.25, recent: [] },
    classification: null, explanations: [], refined: null, cluster_id: null, cluster_group: null,
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

test("clusterChip: names the group and says the rows measure alike — never 'duplicate' (T-320)", () => {
  const group = { cluster_id: "cluster:0199abc", label: "199ABC", rows_in_view: 11 };
  const many = clusterChip(makeRow({ cluster_id: group.cluster_id, cluster_group: group }));
  // The label makes the *grouping* visible: eleven rows carrying "199ABC" are one group, which a
  // bare "seen before" on each of eleven rows never showed.
  assert.deepEqual(many, {
    cls: "cluster",
    text: "signature cluster 199ABC · 11 rows measure alike",
    title: CLUSTER_CHIP_TITLE,
  });
  // The claim is what was measured, not a deduplication verdict: near-duplicate rows are minted
  // upstream by entity resolution and a cluster sets nothing on an emitter, so the chip must not
  // call them duplicates or suggest anything was merged or removed.
  for (const banned of ["duplicate", "merged", "removed", "collapsed"]) {
    assert.ok(!many!.text.toLowerCase().includes(banned), `chip says "${banned}": ${many!.text}`);
  }
  assert.ok(CLUSTER_CHIP_TITLE.includes("stay separate inventory rows"));

  // Alone in view: the group is named, but nothing is claimed about other rows.
  assert.deepEqual(
    clusterChip(makeRow({ cluster_id: group.cluster_id, cluster_group: { ...group, rows_in_view: 1 } })),
    { cls: "cluster", text: "signature cluster 199ABC · seen before", title: CLUSTER_CHIP_TITLE },
  );
  // The count and the label come from the server; the client computes neither.
  assert.equal(clusterChip(makeRow({ cluster_id: null, cluster_group: null })), null);
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

test("decodeActionLabel: names the known scheme, else plain Decode", () => {
  assert.equal(decodeActionLabel({ identity_scheme: "rds-pi" }), "Decode RDS");
  assert.equal(decodeActionLabel({ identity_scheme: null }), "Decode");
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

// ---- the viewed window: Candidates scoped, Confirmed always listed (T-260, ADR-0017 §2.1/§2.2) ----

/** The capture clock the fixtures run on, deliberately far from any wall clock — the fixture that
 * exposed T-379 sat 3.5 days from `Date.now()`, and every window test here has to fail if a browser
 * clock creeps back in. */
const CAPTURE_EDGE_S = 1_789_297_847;

/** A ctx whose client records every path, answers `/api/inventory` per `state=` tab and
 * `/api/coverage` with the given cell state (T-379). */
function windowCtx(
  pages: Partial<Record<"confirmed" | "candidate", Row[]>> = {},
  coverage: "observed" | "unobserved" | "none" = "observed",
) {
  const paths: string[] = [];
  const store = createStore(initialState());
  const client = {
    get: async <T>(path: string): Promise<T> => {
      paths.push(path);
      if (path.startsWith("/api/coverage")) {
        return (coverage === "none" ? { any: { cells: [] } } : { any: { cells: [{ state: coverage }] } }) as T;
      }
      const tab = new URLSearchParams(path.slice(path.indexOf("?") + 1)).get("state") as "confirmed" | "candidate";
      return { entries: pages[tab] ?? [], next_cursor: null } as T;
    },
  } as unknown as AppContext["client"];
  const ctx: AppContext = { store, client, token: "t" };
  const paramsFor = (tab: string) => {
    const p = paths.find((x) => x.includes(`state=${tab}`));
    assert.ok(p, `no ${tab} query was sent`);
    return new URLSearchParams(p.slice(p.indexOf("?") + 1));
  };
  /** Puts the UI at a live edge on the **capture** clock, the way the spectrum stream does. */
  const atLiveEdge = (tS = CAPTURE_EDGE_S) => store.set((s) => ({ live: { ...s.live, edgeTS: tS } }));
  return { ctx, store, paths, paramsFor, atLiveEdge };
}

test("waterfallSpanS: the ring height over the row rate, header rate first, then the device, then the default", () => {
  const base = winState();
  assert.equal(waterfallSpanS(base), FALLBACK_ROWS / DEFAULT_ROW_RATE_HZ, "≈ 20.5 s at 25 rows/s");
  assert.equal(waterfallSpanS(winState({ rowRateHz: 64 })), FALLBACK_ROWS / 64);
  assert.equal(waterfallSpanS(winState({ rowsPerS: 10 })), FALLBACK_ROWS / 10, "the device rate when no header has arrived");
  assert.equal(
    waterfallSpanS(winState({ rowRateHz: 64, rowsPerS: 10 })),
    FALLBACK_ROWS / 64,
    "the stream header wins over the device poll",
  );
});

// ---- T-379: one window, on the capture clock ----

/** A `WindowState` with everything unset, so each test names only what it is about. */
function winState(over: {
  view?: { loHz: number; hiHz: number } | null; rowRateHz?: number | null; edgeTS?: number | null;
  rowsPerS?: number | null; time?: { live: boolean; tS?: number; spanS?: number | null };
  captureWindow?: { t0S: number; t1S: number; spanS: number } | null;
} = {}) {
  return {
    live: { view: over.view ?? null, rowRateHz: over.rowRateHz ?? null, edgeTS: over.edgeTS ?? null },
    device: { rowsPerS: over.rowsPerS ?? null },
    time: over.time ?? { live: true },
    captureWindow: over.captureWindow ?? null,
  };
}

test("liveEdgeS: the stream's own row time, then the capture window, then UNKNOWN — never a browser clock", () => {
  // The bug this is the fix for: the live edge was `Date.now() / 1000`. A replay, the mock SDR on a
  // time-compressed scene, or any source whose stamps are not the host's runs on a clock of its own
  // — the fixture behind this task was 3.5 days off wall time — so a 20 s window ending at
  // browser-now asked about a range the capture never covered.
  const win = { t0S: CAPTURE_EDGE_S - 120, t1S: CAPTURE_EDGE_S, spanS: 120 };
  assert.equal(liveEdgeS(winState({ edgeTS: CAPTURE_EDGE_S + 3, captureWindow: win })), CAPTURE_EDGE_S + 3, "the stream's row time is freshest");
  assert.equal(liveEdgeS(winState({ captureWindow: win })), CAPTURE_EDGE_S, "the capture window when no row has arrived");
  assert.equal(liveEdgeS(winState()), null, "and UNKNOWN when neither has answered — not `now`");
});

test("THE PROPERTY: the Candidate window is the window the waterfall shows, on the capture clock", () => {
  const live = winState({ rowRateHz: 25, edgeTS: CAPTURE_EDGE_S });
  assert.deepEqual(viewWindow(live), { t0: CAPTURE_EDGE_S - FALLBACK_ROWS / 25, t1: CAPTURE_EDGE_S });

  // Scrubbed back: the reviewed instant, over the *same* span the waterfall renders — not a
  // separate review constant. `REVIEW_WINDOW_S = 3600` was a second window: the list answered about
  // an hour while every other surface answered about the 20 s under it.
  const reviewing = winState({ rowRateHz: 25, edgeTS: CAPTURE_EDGE_S, time: { live: false, tS: 500 } });
  assert.deepEqual(viewWindow(reviewing), { t0: 500 - FALLBACK_ROWS / 25, t1: 500 }, "the reviewed instant, not now");

  // A span dragged on the time navigator is the window, on this surface too (T-340).
  const dragged = winState({ rowRateHz: 25, time: { live: false, tS: 500, spanS: 600 } });
  assert.deepEqual(viewWindow(dragged), { t0: 500 - 600, t1: 500 }, "the dragged span, exactly — the waterfall's own historyWindow");
});

test("THE CONTROL: with no live edge reported, the window is UNKNOWN rather than a plausible one", () => {
  // Without this the property is satisfiable by always producing *some* window — which is precisely
  // how the empty sidebar happened: a window was always produced, and it was on the wrong clock.
  assert.equal(viewWindow(winState({ rowRateHz: 25 })), null);
  assert.equal(viewWindow(winState({ rowRateHz: 25, time: { live: true } })), null);
});

test("viewFilters carries the frequency span and deliberately no time (the window belongs to the Candidate query alone)", () => {
  const f = viewFilters(winState({ view: { loHz: 99.6e6, hiHz: 102e6 }, rowRateHz: 25 }));
  // T-587: `relations: "all"` is the other thing every Explore query carries now — see the
  // dedicated test above for why (the `shown` default was hiding artefacts entirely).
  assert.deepEqual(f, { fLoHz: 99.6e6, fHiHz: 102e6, relations: "all" });
});

test("LIVE: the Candidate query carries the waterfall window; the Confirmed query carries no t0/t1 at all", async () => {
  const { ctx, paramsFor } = windowCtx();
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25, edgeTS: CAPTURE_EDGE_S, view: { loHz: 99.6e6, hiHz: 102e6 } } }));
  await loadInventoryRows(ctx, () => {});

  const cand = paramsFor("candidate");
  assert.equal(Number(cand.get("t1")), CAPTURE_EDGE_S, "the candidate window ends at the live edge");
  assert.equal(Number(cand.get("t0")), CAPTURE_EDGE_S - FALLBACK_ROWS / 25, "and starts one waterfall span back");
  assert.equal(cand.get("f_lo"), String(99.6e6), "the frequency span is sent on both lists");

  const conf = paramsFor("confirmed");
  assert.equal(conf.get("t0"), null, "Confirmed is never time-filtered (ADR-0017 §2.2)");
  assert.equal(conf.get("t1"), null, "Confirmed is never time-filtered (ADR-0017 §2.2)");
  assert.equal(conf.get("f_lo"), String(99.6e6), "but it is still scoped to the view's frequency span");
  // T-389: and it names the live edge even while live. Not time-filtered is not the same as not
  // saying when "now" is — see the test below for what omitting it costs.
  assert.equal(Number(conf.get("at")), CAPTURE_EDGE_S, "the Confirmed list names its live edge on the capture clock");
});

test("T-389 — TWO CLOCKS ON ONE SURFACE: the live Confirmed query names its live edge, so its liveness is not the wall clock's", async () => {
  // The bug the user saw as "confirmed presence boxes never draw". `at` was sent only while
  // scrubbed, so a LIVE Confirmed query carried no live edge at all — and `/api/inventory` then
  // scopes an unwindowed row's `presence` against `Timestamp::now()`, the server's wall clock,
  // while the Candidate rows beside it and every box on the waterfall are on the capture clock.
  // Measured against a running server on a replay 312,021 s from wall time, the same three
  // confirmed rows answered:
  //     no `at`  : liveness=ended, last_interval.open=false   <- what the UI actually sent
  //     at=edge  : liveness=live,  last_interval.open=true    <- the truth of that capture instant
  // So the presence box lost its open (amber) edge and the list called a transmitting station
  // silent. This is T-379's defect on the half T-379 did not touch, and T-379's own tests could not
  // catch it: they asserted `at` for the *scrubbed* case and, for the live case, asserted only that
  // `t0`/`t1` were absent — never that a live edge was named at all.
  const { ctx, paramsFor, atLiveEdge } = windowCtx();
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25 } }));
  atLiveEdge(CAPTURE_EDGE_S);
  await loadInventoryRows(ctx, () => {});
  const conf = paramsFor("confirmed");
  assert.equal(Number(conf.get("at")), CAPTURE_EDGE_S);
  // The property, not the value: whatever clock the capture runs on, the Confirmed list asks about
  // THAT instant. A browser-clock answer here would be ~312,000 s away from it.
  assert.ok(Math.abs(Number(conf.get("at")) - Date.now() / 1000) > 1000, "the capture clock, never Date.now()");
  // And it is still the *only* thing carried: `at` scopes, it never selects (docs/api.md).
  assert.equal(conf.get("t0"), null);
  assert.equal(conf.get("t1"), null);
});

// ---- T-389: the list and the boxes derive from ONE filtered collection ----------------------
//
// The user's rule, verbatim: *the list and the liveness/boxes it renders must never disagree.* Both
// surfaces already read `inventory.rows`; that was not enough, because they read it through two
// predicates — the list counted every row of a state, the waterfall additionally demanded a
// presence extent. Two filters that happen to agree today are not an invariant, so the split
// happens once (`renderedInventory`) and each surface takes a field of the result.

/** A geometry wide enough that every row below is in view, so the test is about the split. */
const T389_G: ax.Geometry = { centerHz: 100_000_000, bandwidthHz: 4_000_000, bins: 1024 };
const T389_V: ax.View = { loHz: 98_000_000, hiHz: 102_000_000 };
const iv = (t0: number, t1: number, open = true) => ({ intervals: 1, on_air_s: t1 - t0, last_interval: { t_start_s: t0, t_end_s: t1, open }, liveness: open ? "live" : "ended", ended_t_s: null, silence_s: 0, confidence: 1 });

test("THE STRUCTURAL RULE (T-389): list count = box count + noExtent + focused, over one collection", () => {
  // A deliberately awkward mix: two candidates and two confirmed with extents, one of each without,
  // a focused row (which substitutes its full-height overlay for a box), and a `deleted` row that
  // belongs to neither surface.
  const rows = [
    makeRow({ id: "c1", state: "candidate", f_center_hz: 99_000_000, presence: iv(100, 118) }),
    makeRow({ id: "c2", state: "candidate", f_center_hz: 99_500_000, presence: iv(104, 120) }),
    makeRow({ id: "c3", state: "candidate", f_center_hz: 99_700_000 }),                        // no presence at all
    makeRow({ id: "k1", state: "confirmed", f_center_hz: 100_500_000, presence: iv(60, 120) }),
    makeRow({ id: "k2", state: "confirmed", f_center_hz: 101_000_000, presence: iv(90, 120) }),
    makeRow({ id: "k3", state: "confirmed", f_center_hz: 101_500_000, presence: { ...iv(1, 2), last_interval: null } }),
    makeRow({ id: "gone", state: "deleted", f_center_hz: 100_100_000, presence: iv(100, 120) }),
  ] as Row[];

  for (const focusedId of [null, "k1", "c1", "c3"]) {
    const r = renderedInventory(rows, focusedId);
    const listed = r.listed.candidate.length + r.listed.confirmed.length;
    const boxes = signalMarkBoxes(r.boxed, focusedId);
    // T-445 INVERTED THIS, rather than deleting it. The identity used to carry a third term for the
    // focused row, which substituted a full-height DOM band for its presence box. On the canvas the
    // focused row draws the same mark, heavier — so the identity is the stronger two-term one, and
    // this assertion now fails if that substitution is ever reintroduced.
    assert.equal(
      listed, boxes.length + r.noExtent.length,
      `focus=${focusedId}: every listed row draws a box or carries no extent — no exceptions`,
    );
    if (r.focused?.presence?.last_interval) {
      const f = boxes.find((b) => b.id === r.focused!.id);
      assert.ok(f, "the focused row draws a box too");
      assert.equal(f!.rgba[3], 1, "…and it is the opaque one: heavier, not different");
    }
    // Not merely equal in count: `boxed` is a SUBSET of what the list shows, so the waterfall can
    // never draw a box for something the list does not name. This is the direction the user saw
    // broken — several boxes beside a heading that said "1".
    const names = new Set([...r.listed.candidate, ...r.listed.confirmed].map((x) => x.id));
    for (const b of boxes) assert.ok(names.has(b.id), `box ${b.id} is one of the listed rows`);
    assert.ok(!names.has("gone"), "a deleted row is on neither surface");
    // And there is no longer a third view of the same rows to keep in step: the brackets were the
    // spectrum pane's own frequency marks, drawn by a renderer that no longer exists.
  }
});

test("T-389: a row with no measured extent is DISCLOSED as one, never dropped and never faked", () => {
  // The honest half of the rule. A confirmed station quiet since before the ring, or a legacy row
  // with no presence track, has no timespan to draw — inventing a rectangle for it would be a
  // measurement claim (docs/api.md `presence`). It stays listed and lands in `noExtent`, so the
  // difference between the two counts has a name and a count rather than being inferred from a
  // missing rectangle.
  const quiet = makeRow({ id: "quiet", state: "confirmed" }) as Row;
  const r = renderedInventory([quiet], null);
  assert.deepEqual(r.listed.confirmed.map((x) => x.id), ["quiet"], "still listed — §2.2's safety valve");
  assert.deepEqual(r.boxed, []);
  assert.deepEqual(r.noExtent.map((x) => x.id), ["quiet"]);
  assert.deepEqual(signalMarkBoxes(r.boxed, null), [], "and no box is fabricated for it");
});

test("T-389: an UNSELECTED confirmed row is drawn — it used to have no marker in the spectrum pane at all", () => {
  // The user's symptom (1): "the confirmed box appears only after clicking the row". T-193 replaced
  // the confirmed bracket with the full-height band box; T-261 then narrowed that box to the
  // FOCUSED row and gave every other one the waterfall's presence box. Between them an unselected
  // confirmed row lost every marker in the spectrum pane, and its waterfall box — for a station on
  // air longer than the ring holds — spans the whole pane, so nothing box-shaped shows there
  // either. The bracket is back, and it is the thing that makes an unselected confirmed row
  // visible and clickable.
  const rows = [
    makeRow({ id: "k", state: "confirmed", f_lo_hz: 100_400_000, f_hi_hz: 100_600_000, presence: iv(60, 120) }),
    makeRow({ id: "c", state: "candidate", f_lo_hz: 99_400_000, f_hi_hz: 99_600_000, presence: iv(110, 120) }),
  ] as Row[];
  // T-445: the bracket layer is gone with the spectrum pane, and the property it existed for is
  // now the box's own — an UNSELECTED confirmed row draws a mark, and the confirmed mark is the
  // heavier of the two, because a confirmed emitter is the stronger claim (T-389's ordering).
  const boxes = signalMarkBoxes(rows, null);
  assert.deepEqual(boxes.map((b) => b.id), ["k", "c"], "both are drawn with nothing focused");
  const conf = boxes.find((b) => b.id === "k")!, cand = boxes.find((b) => b.id === "c")!;
  assert.ok(conf.rgba[3] > cand.rgba[3], "confirmed is drawn heavier than candidate");
});

test("T-389: no live edge known yet sends no `at` at all — a made-up 'now' is worse than none", async () => {
  // The same rule `viewWindow` follows: unknown is said, never invented. Here it cannot even be
  // reached through the Candidate path (no edge ⇒ no window ⇒ no queries), so the guard is on
  // `confirmedFilters` directly.
  assert.equal(confirmedFilters(winState({ rowRateHz: 25, time: { live: true } })).at, undefined);
  assert.equal(confirmedFilters(winState({ rowRateHz: 25, edgeTS: CAPTURE_EDGE_S, time: { live: true } })).at, CAPTURE_EDGE_S);
});

test("THE SAFETY VALVE: a confirmed station that has been quiet for hours stays listed while candidates are window-scoped", async () => {
  // The regression this guards against: scoping Confirmed to the window too would make the user's
  // own confirmed stations disappear from Explore the moment they stopped transmitting.
  const nowS = CAPTURE_EDGE_S;
  const quiet = makeRow({ id: "quiet", state: "confirmed", last_seen_s: nowS - 6 * 3600, first_seen_s: nowS - 9 * 3600 });
  const onAir = makeRow({ id: "onair", state: "candidate", last_seen_s: nowS - 2 });
  const { ctx, store, paramsFor, atLiveEdge } = windowCtx({ confirmed: [quiet], candidate: [onAir] });
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25 } }));
  atLiveEdge(nowS);
  await loadInventoryRows(ctx, () => {});

  const rows = store.get().inventory.rows;
  assert.ok(rows.quiet, "the quiet confirmed station is still listed, six hours off the air");
  assert.equal(rows.quiet.state, "confirmed");
  assert.ok(rows.onair, "the live candidate is listed too");

  // The asymmetry is in the request, not in any client-side filtering of the answer: the UI never
  // decides which rows qualify (thin-client rule) — it only chooses whether to send a window.
  assert.equal(paramsFor("confirmed").get("t0"), null);
  const t0 = Number(paramsFor("candidate").get("t0"));
  assert.ok(quiet.last_seen_s < t0, "the quiet station would have been excluded had Confirmed been windowed");
});

test("reviewing: the Candidate window follows the scrubbed instant; Confirmed stays unwindowed there too", async () => {
  const { ctx, paramsFor, atLiveEdge } = windowCtx();
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25 } }));
  atLiveEdge();
  ctx.store.set(reviewAt(CAPTURE_EDGE_S - 9614));
  await loadInventoryRows(ctx, () => {});
  const cand = paramsFor("candidate");
  assert.equal(Number(cand.get("t1")), CAPTURE_EDGE_S - 9614);
  assert.equal(Number(cand.get("t0")), CAPTURE_EDGE_S - 9614 - FALLBACK_ROWS / 25, "the waterfall's own span, not a review constant");
  assert.equal(paramsFor("confirmed").get("t0"), null, "a quiet confirmed station does not vanish while reviewing either");
});

// ---- scrub-back re-derivation (T-263, ADR-0017 TM-7) ----

test("scrubbed back: Confirmed names the scrubbed instant as its live edge, and is still not time-filtered", () => {
  // `t0`/`t1` do two things at once: select rows and scope their `presence`. Confirmed can only
  // have the second — windowing it is the §2.2 regression — so it sends `at` instead. Without it a
  // scrubbed-back Confirmed row would read the liveness it has *now*, disagreeing with every other
  // surface on screen.
  const view = { loHz: 99.6e6, hiHz: 102e6 };
  assert.deepEqual(confirmedFilters(winState({ view, rowRateHz: 25 })), { fLoHz: 99.6e6, fHiHz: 102e6, relations: "all" }, "no `at` at the live edge");
  const scrubbed = confirmedFilters(winState({ view, rowRateHz: 25, time: { live: false, tS: 1_789_540_000 } }));
  assert.equal(scrubbed.at, 1_789_540_000);
  assert.equal(scrubbed.t0, undefined, "still no window: a window would filter the catalogue");
  assert.equal(scrubbed.t1, undefined);
});

test("THE SAFETY VALVE HOLDS WHILE SCRUBBED: a station quiet during the scrubbed window stays listed", async () => {
  const nowS = CAPTURE_EDGE_S, tS = nowS - 6 * 3600;
  const quiet = makeRow({ id: "quiet", state: "confirmed", last_seen_s: nowS - 30, first_seen_s: nowS - 9 * 3600 });
  const { ctx, store, paramsFor, atLiveEdge } = windowCtx({ confirmed: [quiet], candidate: [] });
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25 } }));
  atLiveEdge(nowS);
  ctx.store.set(reviewAt(tS));
  await loadInventoryRows(ctx, () => {});

  assert.ok(store.get().inventory.rows.quiet, "the confirmed catalogue survives the scrub");
  const conf = paramsFor("confirmed");
  assert.equal(conf.get("t0"), null, "not time-filtered, so nothing quiet is dropped");
  assert.equal(Number(conf.get("at")), tS, "but its liveness is re-derived as of the scrubbed instant");
  // The asymmetry still lives in the request, never in client-side filtering of the answer.
  assert.equal(Number(paramsFor("candidate").get("t1")), tS);
});

// ---- T-379: which emptiness is it? ----

test("THE DISTINGUISHING TEST: not-fetched, unobserved and observed-but-quiet are three different states", async () => {
  // The whole point. All three render an empty list, and on the old code all three said "Nothing
  // here yet." Two of them are bugs or non-findings; only the third is a statement about the air.

  // 1. Not fetched: no live edge, so no window — and therefore NO QUERY AT ALL.
  const notFetched = windowCtx({ candidate: [makeRow({ id: "c1", state: "candidate" })] });
  await loadInventoryRows(notFetched.ctx, () => {});
  assert.equal(notFetched.paths.length, 0, "nothing is asked about a window the UI cannot name");
  assert.equal(notFetched.store.get().inventory.window, null, "and the window is recorded as unknown");
  assert.equal(emptyListText(notFetched.store.get().inventory), "Loading…");

  // 2. Unobserved: a window was asked about, and the coverage map says nothing ever looked there.
  const unobserved = windowCtx({ candidate: [] }, "unobserved");
  unobserved.ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25, view: { loHz: 99.6e6, hiHz: 102e6 } } }));
  unobserved.atLiveEdge();
  await loadInventoryRows(unobserved.ctx, () => {});
  assert.equal(unobserved.store.get().inventory.window?.coverage, "unobserved");
  assert.equal(
    emptyListText(unobserved.store.get().inventory),
    "Nothing was observed in this window — no data, not a quiet band.",
  );

  // 3. Observed and quiet: the receiver was listening here and heard nothing. The only finding.
  const quiet = windowCtx({ candidate: [] }, "observed");
  quiet.ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25, view: { loHz: 99.6e6, hiHz: 102e6 } } }));
  quiet.atLiveEdge();
  await loadInventoryRows(quiet.ctx, () => {});
  assert.equal(quiet.store.get().inventory.window?.coverage, "observed");
  assert.equal(emptyListText(quiet.store.get().inventory), "Nothing on the air in this window.");

  // Three states, three sentences — and the three are pairwise different, which is the property a
  // single "Nothing here yet." could never satisfy.
  const texts = [notFetched, unobserved, quiet].map((c) => emptyListText(c.store.get().inventory));
  assert.equal(new Set(texts).size, 3, `three distinguishable empty states, got ${JSON.stringify(texts)}`);
});

test("a coverage answer that never came is UNKNOWN, never 'unobserved'", () => {
  // "Not asked about" must not harden into a measurement claim. An absent answer is the one case
  // where the sidebar says the least, because a wrong "nothing ever looked here" is an
  // absence-of-signal finding invented out of an absence of an HTTP response.
  assert.equal(emptyListText({ window: { coverage: null }, loadedAtS: 1, error: null }), "Nothing listed for this window.");
  assert.equal(emptyListText({ window: null, loadedAtS: 1, error: null }), "Waiting for the capture window…");
  assert.equal(emptyListText({ window: null, loadedAtS: null, error: null }), "Loading…");
  assert.equal(emptyListText({ window: { coverage: "observed" }, loadedAtS: 1, error: "boom" }), "boom", "an error is never dressed as emptiness");
});

test("THE CONTROL THAT MATTERS: a window that DOES hold data renders its rows, not an empty state", async () => {
  // Without this, every assertion above is satisfiable by a sidebar that is always empty and merely
  // explains itself well. This is the empirical finding restated as a test: the fixture behind
  // T-379 had five candidates in the store and four of them inside the 21 s window under the
  // waterfall, while the query the UI actually sent returned zero.
  const rows = [makeRow({ id: "c1", state: "candidate" }), makeRow({ id: "c2", state: "candidate", f_center_hz: 100_100_000 })];
  const { ctx, store, paramsFor, atLiveEdge } = windowCtx({ candidate: rows, confirmed: [] });
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25, view: { loHz: 99.6e6, hiHz: 102e6 } } }));
  atLiveEdge();
  await loadInventoryRows(ctx, () => {});

  const listed = Object.values(store.get().inventory.rows);
  assert.deepEqual(listed.map((r) => r.id).sort(), ["c1", "c2"], "the window's rows are on screen");
  // On values: the window actually sent is the capture clock's, so rows stamped on that clock fall
  // inside it. A browser-clock window would have been ~306,000 s away from these timestamps.
  const t0 = Number(paramsFor("candidate").get("t0")), t1 = Number(paramsFor("candidate").get("t1"));
  for (const r of rows) assert.ok(r.last_seen_s >= t0 - 1e6 && t1 >= t0, `row ${r.id} is of the window that was asked about`);
  assert.ok(Math.abs(t1 - CAPTURE_EDGE_S) < 1e-9, "and that window ends at the capture clock's live edge");
  assert.equal(store.get().inventory.window?.coverage, "observed");
});

test("a non-empty list never pays for a coverage question", async () => {
  const { ctx, paths, atLiveEdge } = windowCtx({ candidate: [makeRow({ id: "c1", state: "candidate" })] });
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25, view: { loHz: 99.6e6, hiHz: 102e6 } } }));
  atLiveEdge();
  await loadInventoryRows(ctx, () => {});
  assert.ok(!paths.some((p) => p.startsWith("/api/coverage")), "coverage is asked only when the answer would change what is said");
});

// ---- T-385: the focus panel's own emptiness ----
//
// T-379 taught the sidebar to say *which* emptiness an empty list is. The focus panel beside it
// still had one sentence for every absence: "That signal is no longer in the inventory." For a
// window-scoped inventory (CLAUDE.md invariant 2) the ordinary absence is having scrubbed or
// retuned away, so that sentence was a claim about the user's data the UI had never measured —
// exactly the failure the exploration-first rule exists to stop, applied to a panel.
//
// The pair of properties, and neither alone is the fix:
//   a. a row outside the window says so, and does NOT say it was deleted;
//   b. a row that really WAS deleted still says so — otherwise this is the same bug mirrored.

test("THE DISTINGUISHING TEST: deleted, outside-the-window and unknown are different states", () => {
  const win = { t0: 100, t1: 120, coverage: "observed" as const };

  // a. The entry exists and is not deleted; it is simply not among this window's rows.
  const away = signalFocus(undefined, win, { state: "candidate" });
  assert.deepEqual(away, { kind: "out-of-window", coverage: "observed" });
  assert.equal(
    signalFocusText(away as Exclude<typeof away, { kind: "row" }>),
    "Not in the window on screen — outside the time or frequency range you are viewing, not gone.",
  );

  // b. THE CONTROL ON THE FIX ITSELF: a real deletion is still reported as one. A fix that turned
  // every absence into "outside the window" would hide the user's own delete behind a reassuring
  // message — the same bug facing the other way.
  const deleted = signalFocus(undefined, win, { state: "deleted" });
  assert.deepEqual(deleted, { kind: "deleted" });
  assert.equal(signalFocusText(deleted as Exclude<typeof deleted, { kind: "row" }>), "That signal was deleted from the inventory.");

  // c. A 404: there is no such entry at all. Also a real absence, and its own sentence.
  const gone = signalFocus(undefined, win, "gone");
  assert.deepEqual(gone, { kind: "gone" });

  // d. A lookup that never answered stays UNKNOWN — it never hardens into "deleted", the same rule
  // T-379 applied to a coverage answer that never came.
  const unchecked = signalFocus(undefined, win, "failed");
  assert.deepEqual(unchecked, { kind: "unchecked" });
  assert.equal(signalFocusText(unchecked as Exclude<typeof unchecked, { kind: "row" }>), "Could not check whether that signal is still listed.");

  // e. Nothing is claimed while the lookup is in flight, or before a window is even known.
  assert.deepEqual(signalFocus(undefined, win, "loading"), { kind: "checking" });
  assert.deepEqual(signalFocus(undefined, null, "loading"), { kind: "no-window" });
  assert.deepEqual(signalFocus(undefined, null, undefined), { kind: "no-window" });

  // The property a single sentence could never satisfy: all five are pairwise different, the way
  // T-379 asserted its three emptinesses were.
  const texts = [
    away, deleted, gone, unchecked,
    signalFocus(undefined, win, "loading"), signalFocus(undefined, null, "loading"),
  ].map((f) => signalFocusText(f as Exclude<typeof f, { kind: "row" }>));
  assert.equal(new Set(texts).size, 6, `six distinguishable absences, got ${JSON.stringify(texts)}`);
});

test("a deletion outranks the window: it is still a deletion while the capture window is unknown", () => {
  // An emitter's existence is not a fact about a window, so a decisive lookup is not withheld
  // pending one. The converse — concluding "deleted" from the row's absence — is what this task
  // removed, and there is no path to it: `signalFocus` reads only the lookup for that claim.
  assert.deepEqual(signalFocus(undefined, null, { state: "deleted" }), { kind: "deleted" });
  assert.deepEqual(signalFocus(undefined, null, "gone"), { kind: "gone" });
});

test("over a window nothing ever sampled, an absent signal is unobserved-here, not gone", () => {
  // T-379's second emptiness, on this surface: where the front end never looked, the row's absence
  // is not evidence about the row at all. And a coverage answer that never came says the least.
  const f = signalFocus(undefined, { t0: 1, t1: 2, coverage: "unobserved" }, { state: "confirmed" });
  assert.equal(
    signalFocusText(f as Exclude<typeof f, { kind: "row" }>),
    "Not in this window — nothing was observed here, so it is unobserved, not gone.",
  );
  const unknown = signalFocus(undefined, { t0: 1, t1: 2, coverage: null }, { state: "confirmed" });
  assert.equal(signalFocusText(unknown as Exclude<typeof unknown, { kind: "row" }>), "Not listed for this window.");
});

test("THE CONTROL THAT MATTERS: a window that DOES hold the emitter renders it, absences unread", () => {
  // Without this, every assertion above is satisfiable by a panel that never shows a signal and
  // merely explains itself beautifully. A present row is rendered from the row itself — the lookup
  // is not consulted, and cannot override what the window actually holds.
  const row = makeRow({ id: "e1", state: "confirmed" });
  assert.deepEqual(signalFocus(row, { t0: 100, t1: 120, coverage: "observed" }, "failed"), { kind: "row", row });
  assert.deepEqual(signalFocus(row, null, { state: "deleted" }), { kind: "row", row }, "the row on screen wins over a stale lookup");
});

test("fetchEmitterLookup: reads the entry's own lifecycle state; a 404 is `gone`, any other failure is unknown", async () => {
  // `GET /api/inventory/{id}` serves deleted entries too (docs/api.md), which is the only thing
  // that can tell a deletion from an absence; nothing is inferred from the row's absence itself.
  let asked: string | null = null;
  const live = fakeClient({ get: (p) => { asked = p; return { id: "e 1", state: "candidate" }; } });
  assert.deepEqual(await fetchEmitterLookup(live, "e 1"), { state: "candidate" });
  assert.equal(asked, "/api/inventory/e%201", "the id is encoded into the path");

  const del = fakeClient({ get: () => ({ id: "e1", state: "deleted" }) });
  assert.deepEqual(await fetchEmitterLookup(del, "e1"), { state: "deleted" });

  const missing = fakeClient({ get: () => { throw { status: 404, code: "not_found", message: "no such inventory entry" }; } });
  assert.equal(await fetchEmitterLookup(missing, "e1"), "gone");

  const offline = fakeClient({ get: () => { throw { status: 503, code: "unavailable", message: "no signal inventory" }; } });
  assert.equal(await fetchEmitterLookup(offline, "e1"), "failed", "a server that could not answer never means deleted");
});

// ---- T-386: the selections sidebar is a view of the window --------------------------------
//
// It used to render `selections.list` whole — every frequency, all time — beside a waterfall
// showing one band's twenty seconds, and it loaded once and never re-read the cursor. That is the
// whole-UI window rule broken by *widening*: the panel looked full by answering a bigger question
// than the one on screen.

const sel = (over: Partial<Selection> = {}): Selection => ({
  id: "s1", name: "region", f_lo: 101.2e6, f_hi: 101.4e6, tags: [], links: [],
  created: CAPTURE_EDGE_S, updated: CAPTURE_EDGE_S, ...over,
});

const VIEW = { loHz: 100e6, hiHz: 102e6 };
const WINDOW = { t0: CAPTURE_EDGE_S - 20, t1: CAPTURE_EDGE_S };

test("T-386 THE CONTROL THAT MATTERS: a window that DOES hold selections renders them, asserted on ids", () => {
  // Without this every assertion below is satisfiable by a sidebar that shows nothing and explains
  // itself beautifully. Both kinds are here: a timed selection inside the window, and an untimed
  // one whose frequency overlaps the view.
  const inside = sel({ id: "timed-in", t_lo: CAPTURE_EDGE_S - 10, t_hi: CAPTURE_EDGE_S - 5 });
  const anytime = sel({ id: "untimed-in", f_lo: 100.5e6, f_hi: 100.7e6 });
  const split = selectionsInWindow([inside, anytime], VIEW, WINDOW);
  assert.deepEqual(split.listed.map((s) => s.id), ["timed-in", "untimed-in"]);
  assert.equal(split.outside, 0);
  assert.equal(split.undecidable, 0);
  assert.equal(selectionsEmptyText(split), "Drag across the waterfall to mark a region.");
});

test("T-386 THE PROPERTY: a selection outside this (time × frequency) window is not listed, and is disclosed as elsewhere", () => {
  const otherBand = sel({ id: "far", f_lo: 915.1e6, f_hi: 915.3e6 });
  const otherTime = sel({ id: "long-ago", t_lo: CAPTURE_EDGE_S - 4000, t_hi: CAPTURE_EDGE_S - 3900 });
  const split = selectionsInWindow([otherBand, otherTime], VIEW, WINDOW);
  assert.deepEqual(split.listed, []);
  assert.equal(split.outside, 2);
  // Elsewhere, never gone: the empty state names the count and says how to reach them.
  assert.match(selectionsEmptyText(split), /2 selections, none in this window/);
});

test("T-386: an UNTIMED selection is in every window its frequency overlaps — filtering it out would be the same bug pointing the other way", () => {
  // A frequency-only mark claims no time extent ("this band, whenever"), so dropping it on a time
  // window it never claimed is "we have it but didn't render it" — the failure the rule names.
  const anytime = sel({ id: "untimed" });
  for (const w of [WINDOW, { t0: 0, t1: 1 }, null]) {
    assert.deepEqual(selectionsInWindow([anytime], VIEW, w).listed.map((s) => s.id), ["untimed"], `window ${JSON.stringify(w)}`);
  }
  // And it is still frequency-scoped: another band is another window.
  assert.deepEqual(selectionsInWindow([anytime], { loHz: 900e6, hiHz: 930e6 }, WINDOW).listed, []);
});

test("T-386: with no window known a TIMED selection is undecidable, never 'outside' — and the list says so in the shared words", () => {
  // The third state T-379 exists for. "No window was asked about" is not a finding about where the
  // selection is, and the sentence is the one every window-scoped surface shares (T-387).
  const timed = sel({ id: "timed", t_lo: 1, t_hi: 2 });
  const split = selectionsInWindow([timed], VIEW, null);
  assert.deepEqual(split, { listed: [], outside: 0, undecidable: 1 });
  assert.equal(selectionsEmptyText(split), WAITING_FOR_WINDOW);
  assert.equal(selectionsEmptyText(split, "boom"), "boom", "an error outranks every window state");
});

test("T-386: no view known filters nothing on frequency, rather than everything", () => {
  const a = sel({ id: "a", f_lo: 1e6, f_hi: 2e6 }), b = sel({ id: "b", f_lo: 915e6, f_hi: 916e6 });
  assert.deepEqual(selectionsInWindow([a, b], null, WINDOW).listed.map((s) => s.id), ["a", "b"]);
});

test("T-386/T-389: the sidebar list and the surface's marks come from ONE collection, filtered ONCE", () => {
  // The original form of this test held `live-spectrum.ts` to `selectionsInWindow`'s `listed` for
  // both its full-height boxes and its timed ones, because two parallel filters that happen to
  // agree are not an invariant. T-445 made the second filter unnecessary rather than correct: the
  // surface hands `s.selections.list` and `s.inventory.rows` straight to `marks.ts`, and the only
  // thing that decides whether a mark is on screen is `markQuads` clipping it to the pane's OWN
  // box — the same box the tiles were drawn in, on the same frame. A mark cannot be drawn where
  // the pane is not looking, so there is nothing left for a window filter to disagree with.
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  // T-522 moved the composition into `paneMarkBoxes` (so the found-signal toggle has one place to
  // gate), but the rows and selections still flow straight from the store, through it, to
  // `signalMarkBoxes`/`selectionMarkBoxes` — no second filter appeared.
  assert.match(src, /paneMarkBoxes\(Object\.values\(s\.inventory\.rows\).*s\.selections\.list/,
    "the marks are composed from the store's rows and selections, straight through");
  // T-1004 added the linked-focus flag (a pane that does not own the selection draws its ghost); the
  // rows and the focus still go straight through to the one `signalMarkBoxes`.
  assert.match(src, /signalMarkBoxes\(rows, focusId, undefined, undefined, linkedFocus\)/, "…into the same signalMarkBoxes…");
  assert.match(src, /selectionMarkBoxes\(sels, selId, paneBox, linkedFocus\)/, "…and the same selectionMarkBoxes");
  assert.match(src, /markQuads\(boxesFor\(pane\), edge, pane\.box, pane\.rect\)/,
    "placed in the pane's own box and rect — the renderer's mapping, not a second one");
  // And the sidebar's window is the pane's window: `mirror()` is the ONE writer of `live.view`
  // here, so the list and the canvas cannot be scoped to two different things.
  const writers = [...src.matchAll(/live: \{ \.\.\.s\.live, view:/g)].length;
  assert.equal(writers, 1, "exactly one place publishes the viewport as the app's view window");

  const idx = readFileSync("src/app/explore/index.ts", "utf8");
  // The whole window, both axes: a split asked for with `null, null` would satisfy the shape and
  // render the same all-time list the panel used to.
  assert.match(
    idx,
    /selectionsInWindow\(sortSelections\(s\.selections\.list\), centreView\(s\), viewWindow\(s\)\)/,
    "the sidebar renders the split, over this view and this window",
  );
  assert.match(idx, /list\.replaceChildren\(\.\.\.\(split\.listed\.length/, "and renders `listed`, not the raw list");
  // And it re-reads when either axis of the window moves, not only when the selections do (T-384).
  assert.match(idx, /windowKey\(s\)\}\|\$\{centreViewKey\(s\)\}/);
});

test("T-386 CLOCK GUARD: no clock of the browser's own reaches the Explore sidebar modules", () => {
  // T-393's guard, extended to the modules T-386 touched. `viewWindow`/`liveEdgeS` already put
  // this sidebar on the capture clock (T-379); this is the structural half, so the window the
  // selections and the lists are filtered against cannot regress to wall time in a later edit.
  const clocks = ["Date.now", "performance.now", "toLocaleTimeString", "getTimezoneOffset"];
  const bare = (f: string) => readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  for (const f of ["src/app/explore/selections.ts", "src/app/explore/index.ts"]) {
    for (const word of clocks) assert.ok(!bare(f).includes(word), `${f} must not contain "${word}"`);
  }
  // `inventory.ts` keeps exactly one `Date.now`, and it is not a capture time: `loadedAtS` records
  // whether this *page* has completed a load, which is what tells "Loading…" from an empty answer.
  // Nothing measured, windowed or compared against a capture stamp may use it — so it is named
  // rather than exempted wholesale, and the other three clocks are banned outright.
  const inv = bare("src/app/explore/inventory.ts");
  for (const word of clocks.slice(1)) assert.ok(!inv.includes(word), `inventory.ts must not contain "${word}"`);
  assert.equal(inv.match(/Date\.now/g)?.length, 1, "the one page-lifecycle stamp, and no second clock");
  assert.match(inv, /setInventoryRows\(rows, Date\.now\(\) \/ 1000\)/, "and it is that one");
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

// ---------------------------------------------------------------------------
// T-458: where a region stroke goes
// ---------------------------------------------------------------------------
//
// T-193's user-band override was left with **no setter at all** when T-456 gave the drag to
// panning — stored state that `surface/marks.ts` still honoured (it draws the override in place of
// the measured extent) and nothing could cause. The override is kept, and shift+drag is its input.
// These are the two destinations one gesture now has, and the branch between them, because a branch
// that is wrong in either direction is silent: a stroke that rewrites a signal's band when the user
// meant a selection, or one that makes a selection when they had just asked to adjust a band.

const REGION = { f0Hz: 101.2e6, f1Hz: 101.4e6, t0Ns: 1_789_300_000e9, t1Ns: 1_789_300_005e9 };
const MHz = (hz: number) => `${(hz / 1e6).toFixed(4)} MHz`;

function regionCtx(over: { put?: (p: string, b?: unknown) => unknown } = {}) {
  const store = createStore(initialState());
  const calls: { method: string; path: string; body?: unknown }[] = [];
  const client = {
    put: async <T>(path: string, body?: unknown): Promise<T> => {
      calls.push({ method: "PUT", path, body });
      if (over.put) return over.put(path, body) as T;
      throw new Error(`unexpected PUT ${path}`);
    },
    post: async <T>(path: string, body?: unknown): Promise<T> => { calls.push({ method: "POST", path, body }); return {} as T; },
    del: async <T>(path: string): Promise<T> => { calls.push({ method: "DELETE", path }); return {} as T; },
  };
  return { store, calls, ctx: { store, client } as unknown as AppContext };
}

test("regionDestination: unarmed is a selection; armed is that row's band; armed-but-gone is NEITHER", () => {
  const rows = { e1: makeRow({ id: "e1" }) };
  assert.deepEqual(regionDestination(null, rows), { kind: "selection" });
  assert.deepEqual(regionDestination("e1", rows), { kind: "band", id: "e1" });
  // The inventory is scoped to the viewed window, so a user can arm "Adjust band", scrub away and
  // then stroke. Falling back to "selection" there would silently do something else with the
  // stroke; falling back to "band" would write an override for a row we can no longer see.
  assert.deepEqual(regionDestination("gone", rows), { kind: "stale", id: "gone" });
});

test("regionIsReal refuses what clamping to the pane could have flattened", () => {
  assert.equal(regionIsReal(REGION), true);
  assert.equal(regionIsReal({ ...REGION, f1Hz: REGION.f0Hz }), false);
  assert.equal(regionIsReal({ ...REGION, t1Ns: REGION.t0Ns }), false);
});

test("commitRegion ARMED: PUTs the stroke as that row's band, patches the row, and DISARMS", async () => {
  const entry = makeRow({ id: "e1", user_band: { f_lo: REGION.f0Hz, f_hi: REGION.f1Hz, set_at: 1, actor: "fp", reason: null, reason_withheld: false } });
  const { store, calls, ctx } = regionCtx({ put: () => ({ user_band: entry.user_band, entry }) });
  store.set(setInventoryRows({ e1: makeRow({ id: "e1" }) }, 0));
  store.set(setBandEdit("e1"));

  commitRegion(ctx, REGION, MHz);
  assert.equal(store.get().bandEdit, null, "the arming must clear on the stroke, not linger for the next one");
  await new Promise((r) => setTimeout(r, 0));

  assert.deepEqual(calls, [{ method: "PUT", path: "/api/inventory/e1/band", body: { f_lo: REGION.f0Hz, f_hi: REGION.f1Hz } }]);
  assert.deepEqual(store.get().inventory.rows.e1.user_band, entry.user_band, "the override lands without waiting for a poll");
  assert.match(store.get().toast.text, /^Band set: /);
  assert.deepEqual(store.get().focus, { kind: "none" }, "an armed stroke must not ALSO make and focus a selection");
});

test("commitRegion ARMED and refused: the SERVER's reason is shown, and the arming is still cleared", async () => {
  const { store, ctx } = regionCtx({ put: () => { throw { code: "invalid", message: "band does not overlap the measured extent" }; } });
  store.set(setInventoryRows({ e1: makeRow({ id: "e1" }) }, 0));
  store.set(setBandEdit("e1"));
  commitRegion(ctx, REGION, MHz);
  await new Promise((r) => setTimeout(r, 0));
  assert.match(store.get().toast.text, /band does not overlap the measured extent/);
  // An override left armed after a failed commit would fire on the user's next unrelated stroke —
  // the same class of problem as the setter-less override: state acting when nobody asked it to.
  assert.equal(store.get().bandEdit, null);
});

test("commitRegion ARMED for a row that scrolled out: no PUT, no selection, and it says so", async () => {
  const { store, calls, ctx } = regionCtx();
  store.set(setBandEdit("gone"));
  commitRegion(ctx, REGION, MHz);
  await new Promise((r) => setTimeout(r, 0));
  assert.deepEqual(calls, [], "neither destination is right, so neither is taken");
  assert.match(store.get().toast.text, /no longer in this window/);
  assert.equal(store.get().bandEdit, null);
});

test("commitRegion UNARMED: the stroke becomes a selection with BOTH extents, and focuses it", () => {
  // The other destination, and the one the whole gesture exists for. A selection carries the time
  // extent too — a region is a time-frequency event, never a band "for all time" (CLAUDE.md's
  // signal model) — so the seconds are asserted, not just the edges.
  const { store, ctx } = regionCtx();
  assert.equal(store.get().bandEdit, null);
  commitRegion(ctx, REGION, MHz);

  // Read back through the store that was written, not through `state.selections`: the page's
  // `SelectionStore` is a module singleton whose subscriber is bound to whichever context created
  // it, so mirroring into *this* context's state is an accident of which test ran first. The focus
  // write is this context's own, so that is asserted directly.
  const focus = store.get().focus;
  assert.equal(focus.kind, "selection", "the new region is what you are now looking at");
  const sel = selectionStoreFor(ctx).get(focus.kind === "selection" ? focus.id : "");
  assert.ok(sel, "an unarmed stroke must make a selection");
  assert.equal(sel.f_lo, REGION.f0Hz);
  assert.equal(sel.f_hi, REGION.f1Hz);
  assert.equal(sel.t_lo, REGION.t0Ns / 1e9);
  assert.equal(sel.t_hi, REGION.t1Ns / 1e9);
  assert.match(store.get().toast.text, /^Region: /);
});

test("commitRegion UNARMED: a degenerate stroke commits nothing at all", () => {
  const { store, calls, ctx } = regionCtx();
  commitRegion(ctx, { ...REGION, f1Hz: REGION.f0Hz }, MHz);
  assert.deepEqual(calls, []);
  assert.deepEqual(store.get().focus, { kind: "none" }, "nothing was made, so nothing is focused");
  assert.equal(store.get().toast.text, "");
});

// ---- T-587: an artefact reads as a LABELLED artefact, not a mystery signal --------------------
//
// The field report: signals appeared and vanished with the tuned centre and there was no way to
// tell a receiver artefact from a real emission. The backend already computes the answer
// (`relation.kind` `artifact-of`/`retune-sibling-of`, docs/api.md) — this is presentation only.

/** A `relation` fixture, defaulting to a T-307 image claim with a full backend-rendered reason. */
function makeRelation(over: Partial<NonNullable<Row["relation"]>> = {}): NonNullable<Row["relation"]> {
  return {
    kind: "artifact-of", artifact: "image", source_id: "e-source", author: "system",
    actor: "hk-pipeline/artifact@1", t_s: 1_789_300_000,
    reason: "image of the 100.8 MHz carrier", score: null,
    detail: { n: 1, lo_hz: 99_950_000, predicted_hz: 99_600_000, error_hz: 1200, receive_chain: { device_id: "hackrf-0", antenna_port: null } },
    ...over,
  };
}

test("explanationState: THE THREE STATES — artifact, real, undecided — read from already-asserted fields only", () => {
  assert.equal(explanationState(makeRow({ state: "candidate", relation: makeRelation() })), "artifact");
  assert.equal(explanationState(makeRow({ state: "confirmed", relation: makeRelation({ kind: "retune-sibling-of", artifact: null }) })), "artifact");
  assert.equal(explanationState(makeRow({ state: "confirmed", relation: null })), "real", "a verified emitter with no artifact claim");
  assert.equal(explanationState(makeRow({ state: "candidate", relation: null })), "undecided", "a hypothesis, not yet an artifact and not yet confirmed");
  // A `suppressed-by`/`duplicate-of` relation is a DIFFERENT claim (T-219: same emission, not a
  // receiver artefact) — it must never borrow the artefact label.
  assert.equal(explanationState(makeRow({ state: "candidate", relation: makeRelation({ kind: "suppressed-by", artifact: null }) })), "undecided");
  assert.equal(explanationState(makeRow({ state: "candidate", relation: makeRelation({ kind: "duplicate-of", artifact: null }) })), "undecided");
});

test("explanationChip/explanationReasonText: the backend's own words reach the row, verbatim — nothing re-derived client-side", () => {
  const row = makeRow({ state: "candidate", relation: makeRelation({ reason: "IM3 product of A and B" }) });
  const chip = explanationChip(row);
  assert.ok(chip);
  assert.equal(chip!.cls, "artifact", "its own chip class — never known/unknown/flag/cluster");
  assert.match(chip!.text, /image/);
  assert.equal(chip!.title, "IM3 product of A and B", "hover repeats it, but is not the only place it shows");
  assert.equal(explanationReasonText(row), "IM3 product of A and B", "and the visible text is exactly this, not a client paraphrase");

  const retune = makeRow({ state: "candidate", relation: makeRelation({ kind: "retune-sibling-of", artifact: null, reason: "same LO-relative line seen from 3 tuning centres" }) });
  assert.match(explanationChip(retune)!.text, /retune artefact/);
  assert.equal(explanationReasonText(retune), "same LO-relative line seen from 3 tuning centres");

  // A real emission and an undecided candidate render NEITHER a chip nor a reason line — the
  // honesty constraint the other direction: nothing is labelled that the backend did not assert.
  assert.equal(explanationChip(makeRow({ state: "confirmed", relation: null })), null);
  assert.equal(explanationReasonText(makeRow({ state: "confirmed", relation: null })), null);
  assert.equal(explanationChip(makeRow({ state: "candidate", relation: null })), null);
  assert.equal(explanationReasonText(makeRow({ state: "candidate", relation: null })), null);
});

test("explanationState: RetuneSlope::Absolute is never the source of an artifact label (the weak-claim rule)", () => {
  // The ticket's own guard: `Absolute` is "not LO-relative", never "real" — but the backend
  // (`hk_model::repo::retune`) never claims a `retune-sibling-of` relation from it, so a row with
  // NO relation at all must read `undecided`/`real`, never `artifact`, however it was classified
  // internally. This is the client-side half of that guard: nothing here upgrades a bare `null`
  // relation into an artifact claim.
  assert.equal(explanationState(makeRow({ state: "candidate", relation: null })), "undecided");
  assert.equal(explanationState(makeRow({ state: "confirmed", relation: null })), "real");
});

test("viewFilters: asks relations=all — the T-219 'shown' default was hiding the very artefacts T-587 must surface", () => {
  const f = viewFilters(winState());
  assert.equal(f.relations, "all", "RED without the fix: the default 'shown' filter hides artifact-of/retune-sibling-of rows entirely");
});

test("loadInventoryRows: THE REQUEST — both Candidate and Confirmed queries carry relations=all on the wire", async () => {
  const { ctx, paramsFor, atLiveEdge } = windowCtx();
  ctx.store.set((s) => ({ live: { ...s.live, rowRateHz: 25 } }));
  atLiveEdge(CAPTURE_EDGE_S);
  await loadInventoryRows(ctx, () => {});
  assert.equal(paramsFor("candidate").get("relations"), "all");
  assert.equal(paramsFor("confirmed").get("relations"), "all");
});

test("renderedInventory: an artifact-of row IS listed and boxed (T-587) — suppressed-by/duplicate-of stay hidden (T-219, unchanged)", () => {
  const rows = [
    makeRow({ id: "art", state: "candidate", f_center_hz: 99_600_000, presence: iv(100, 118), relation: makeRelation() }),
    makeRow({ id: "sup", state: "candidate", f_center_hz: 99_601_000, presence: iv(100, 118), relation: makeRelation({ kind: "suppressed-by", artifact: null }) }),
    makeRow({ id: "dup", state: "candidate", f_center_hz: 99_602_000, presence: iv(100, 118), relation: makeRelation({ kind: "duplicate-of", artifact: null }) }),
    makeRow({ id: "plain", state: "candidate", f_center_hz: 99_700_000, presence: iv(100, 118), relation: null }),
  ] as Row[];
  const r = renderedInventory(rows, null);
  assert.deepEqual(r.listed.candidate.map((x) => x.id).sort(), ["art", "plain"], "RED without the fix: the artefact row used to be hidden entirely");
  assert.deepEqual(r.boxed.map((b) => b.id).sort(), ["art", "plain"]);
});

test("signalMarkBoxes: an explained artifact draws ARTIFACT_MARK, on either tab — never the plain signal inks", () => {
  const rows = [
    makeRow({ id: "art-cand", state: "candidate", f_lo_hz: 99_500_000, f_hi_hz: 99_700_000, presence: iv(100, 118), relation: makeRelation() }),
    makeRow({ id: "art-conf", state: "confirmed", f_lo_hz: 100_500_000, f_hi_hz: 100_700_000, presence: iv(100, 118), relation: makeRelation({ kind: "retune-sibling-of", artifact: null }) }),
    makeRow({ id: "plain-cand", state: "candidate", f_lo_hz: 101_500_000, f_hi_hz: 101_700_000, presence: iv(100, 118), relation: null }),
    makeRow({ id: "plain-conf", state: "confirmed", f_lo_hz: 102_500_000, f_hi_hz: 102_700_000, presence: iv(100, 118), relation: null }),
    makeRow({ id: "sup", state: "candidate", f_lo_hz: 103_500_000, f_hi_hz: 103_700_000, presence: iv(100, 118), relation: makeRelation({ kind: "suppressed-by", artifact: null }) }),
  ] as Row[];
  const boxes = signalMarkBoxes(rows, null);
  assert.deepEqual(boxes.map((b) => b.id).sort(), ["art-cand", "art-conf", "plain-cand", "plain-conf"], "suppressed-by still draws nothing");
  assert.deepEqual(boxes.find((b) => b.id === "art-cand")!.rgba, ARTIFACT_MARK);
  assert.deepEqual(boxes.find((b) => b.id === "art-conf")!.rgba, ARTIFACT_MARK, "confirmed does not upgrade an artifact claim either");
  assert.deepEqual(boxes.find((b) => b.id === "plain-cand")!.rgba, CANDIDATE_MARK);
  assert.deepEqual(boxes.find((b) => b.id === "plain-conf")!.rgba, CONFIRMED_MARK);
  assert.notDeepEqual(ARTIFACT_MARK, CANDIDATE_MARK, "the artifact ink must be visually distinct from both signal inks");
  assert.notDeepEqual(ARTIFACT_MARK, CONFIRMED_MARK);
});
