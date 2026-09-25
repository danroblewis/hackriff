// T-804 (MAP-04): the detail sheet — what selecting a signal shows in the T-803 bottom sheet. The
// view-model (`src/app/explore/detail.ts`) is pure, so it is tested here against rows shaped like
// docs/api.md `/api/inventory` serves them (AWARE-053: a signal is a time–frequency region whose
// measurement carries its time). The thin-client rule is asserted on the source and on a spy: building
// the sheet's action row and selecting a signal reach no route; only pressing an action does.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  agoText, clockText, DETAIL_ACTIONS, detailActions, detailFreq, extentText, livenessLine,
  measuredBlock, spanText, type DetailRow,
} from "../src/app/explore/detail";
import { focusSheetTitle } from "../src/app/chrome/focus-sheet";
import { signalMenuItems } from "../src/app/menu";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { focusSignal } from "../src/app/explore/slice";

// 2026-09-13 12:00:00 UTC on the capture clock.
const T0 = Date.UTC(2026, 8, 13, 12, 0, 0) / 1000;

function row(over: Partial<DetailRow> = {}): DetailRow {
  return {
    id: "e1", state: "confirmed",
    f_center_hz: 100_800_000, bandwidth_hz: 181_400, f_lo_hz: 100_709_300, f_hi_hz: 100_890_700,
    first_seen_s: T0 - 600, last_seen_s: T0, count: 42,
    known_status: "known", status: null, tags: [], family: "wfm",
    identity_scheme: null, identity_class: null, withheld: false,
    recurrence: null, classification: null, explanations: [], refined: null,
    cluster_id: null, cluster_group: null, user_band: null, relation: null,
    presence: {
      intervals: 1, on_air_s: 30, liveness: "live", ended_t_s: null,
      last_interval: { t_start_s: T0 - 30, t_end_s: T0 - 1, open: true, revoked_s: 0 },
    },
    measured: { snr_db: 21.4, peak_dbfs: -18.25, t_start_s: T0 - 3, t_end_s: T0 - 2, duration_s: 1 },
    ...over,
  } as DetailRow;
}

test("time words: bursts in ms, clocks in UTC, 'ago' only against the capture clock's live edge", () => {
  assert.equal(spanText(0.04), "40 ms", "a one-off burst is never '0 s'");
  assert.equal(spanText(1), "1.0 s");
  assert.equal(spanText(42), "42 s");
  assert.equal(spanText(300), "5 min");
  assert.equal(spanText(-1), "—");
  assert.equal(clockText(T0), "12:00:00 UTC");
  assert.equal(agoText(T0 - 12, T0), "12 s ago");
  assert.equal(agoText(T0, T0), "just now");
  assert.equal(agoText(T0 - 12, null), null, "no live edge: no 'ago', never the browser's clock");
});

test("the big frequency is the served one: refined when the server refined it, else measured", () => {
  assert.deepEqual(detailFreq(row()), { centerHz: 100_800_000, bandwidthHz: 181_400 });
  const refined = row({ refined: { center_hz: 100_799_880, bandwidth_hz: 180_000 } as DetailRow["refined"] });
  assert.deepEqual(detailFreq(refined), { centerHz: 100_799_880, bandwidthHz: 180_000 });
});

test("measurements carry the time they were measured over (the `measured` object)", () => {
  const m = measuredBlock(row(), T0);
  const byLabel = Object.fromEntries(m.lines.map((l) => [l.label, l.value]));
  assert.equal(byLabel.Centre, "100.8000 MHz");
  assert.equal(byLabel.Bandwidth, "181.4 kHz");
  assert.equal(byLabel.SNR, "21.4 dB");
  assert.equal(byLabel.Peak, "-18.3 dBFS");
  assert.equal(byLabel["In window"], "1 event · 30 s on air");
  assert.equal(m.at, "measured at 11:59:57 UTC over 1.0 s · 2.0 s ago");
  // Nothing measured: no level is shown at all (never a zero), and the time line is absent.
  const none = measuredBlock(row({ measured: null }), T0);
  assert.equal(none.at, null);
  assert.ok(!none.lines.some((l) => l.label === "SNR" || l.label === "Peak"), "a level never appears without its time");
  // A server predating T-350 (no key at all) reads the same as `null`.
  const old = row();
  delete (old as { measured?: unknown }).measured;
  assert.equal(measuredBlock(old, T0).at, null);
});

test("liveness is the served presence: [start, end?], ongoing by default, a detected end provisional", () => {
  const live = livenessLine(row(), T0);
  assert.equal(live.kind, "live");
  assert.equal(live.text, "On air · since 11:59:30 UTC (30 s ago)");
  assert.equal(extentText(row()), "11:59:30 UTC → live (measured to 11:59:59 UTC)",
    "an open interval runs to the live edge; the cap past t_end is labelled as assumption");

  const ended = row({ presence: {
    intervals: 2, on_air_s: 4.2, liveness: "ended", ended_t_s: T0 - 240,
    last_interval: { t_start_s: T0 - 250, t_end_s: T0 - 240, open: false, revoked_s: 2 },
  } });
  const e = livenessLine(ended, T0);
  assert.equal(e.kind, "ended");
  assert.match(e.text, /^Ended 11:56:00 UTC \(4 min ago\) — provisional, reopens if it resumes$/);
  assert.equal(extentText(ended), "11:55:50 UTC → 11:56:00 UTC · 8.0 s on air", "revoked silence is not air time");

  const absent = row({ presence: { intervals: 0, on_air_s: 0, liveness: "absent", ended_t_s: null, last_interval: null } });
  assert.equal(livenessLine(absent, T0).kind, "absent");
  assert.equal(extentText(absent), null, "no interval, no fabricated extent");
  assert.equal(livenessLine(row({ presence: undefined }), T0).kind, "unreported");
});

test("the peek strip names the selected signal's served centre", () => {
  assert.equal(focusSheetTitle({ kind: "signal", id: "e1" }, 100_800_000), "Selected signal · 100.8000 MHz");
  assert.equal(focusSheetTitle({ kind: "signal", id: "e1" }, null), "Selected signal", "not loaded yet: no invented frequency");
});

function spyCtx() {
  const calls: string[] = [];
  const client = new Proxy({}, { get: (_t, k) => (...a: unknown[]) => { calls.push(`${String(k)} ${JSON.stringify(a)}`); return Promise.resolve({}); } });
  const origFetch = globalThis.fetch;
  globalThis.fetch = ((...a: unknown[]) => { calls.push(`fetch ${JSON.stringify(a[0])}`); return Promise.reject(new Error("spy")); }) as typeof fetch;
  const ctx = { store: createStore(initialState()), client, token: "t" } as unknown as AppContext;
  return { ctx, calls, restore: () => { globalThis.fetch = origFetch; } };
}

test("the action row is the context menu's own actions, in the mockup's order", () => {
  const spy = spyCtx();
  try {
    const cand = detailActions(signalMenuItems(spy.ctx, row({ state: "candidate" })));
    assert.deepEqual(cand.map((a) => a.label), ["Listen", "Decode", "Record clip", "Stream out", "Analyze", "Promote", "Delete"]);
    assert.equal(cand[0].primary, true, "Listen is the primary action");
    assert.equal(cand.find((a) => a.id === "delete")?.danger, true);
    const conf = detailActions(signalMenuItems(spy.ctx, row({ identity_scheme: "rds-pi" })));
    assert.deepEqual(conf.map((a) => a.id), ["listen", "decode", "export", "stream", "analyze", "delete"], "no Promote on a confirmed row");
    assert.equal(conf[1].label, "Decode RDS");
    assert.deepEqual(DETAIL_ACTIONS.map((a) => a.id), ["listen", "decode", "export", "stream", "analyze", "promote", "delete"]);
    // SPY: selecting the signal and building its sheet's actions reach nothing; a route is only
    // ever reached by pressing an action, which is an explicit act.
    spy.ctx.store.set((s) => ({ inventory: { ...s.inventory, rows: { e1: row() } } }));
    spy.ctx.store.set(focusSignal("e1"));
    detailActions(signalMenuItems(spy.ctx, row()));
    assert.deepEqual(spy.calls, [], "selecting / building the sheet reached the client");
    conf.find((a) => a.id === "analyze")!.onSelect();
    assert.equal(spy.calls.length, 1);
    assert.match(spy.calls[0], /^post \["\/api\/analyze",\{"emitter_id":"e1"\}\]$/);
    assert.ok(!spy.calls.some((c) => /\/api\/control\//.test(c)), "no device route");
  } finally {
    spy.restore();
  }
});

test("thin client: the detail view-model imports nothing that can reach the backend", () => {
  const src = readFileSync("src/app/explore/detail.ts", "utf8");
  assert.doesNotMatch(src, /^import (?!type )/m, "types only");
  assert.doesNotMatch(src, /\bfetch\(|WebSocket|ctx\.client|Date\.now\(\)/, "no route, and no browser clock");
});
