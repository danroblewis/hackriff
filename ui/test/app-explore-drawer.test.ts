// T-814 (MAP-14): the Explore drawer's pure model + thin-client guard (AWARE-042).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { groupItems, peekLine, quietItems, strongestItem, surveyItems, unknownItems, type EventsResp } from "../src/app/chrome/explore-drawer";
import type { SchedulerResponse } from "../src/scheduler";

test("unknown emitters lead, newest first, one per emitter; known ones are not listed", () => {
  const r: EventsResp = {
    events: [
      { emitter_id: "a", t_start_s: 10, t_end_s: null, open: true, count: 1 },
      { emitter_id: "b", t_start_s: 20, t_end_s: 21, open: false, count: 3 },
      { emitter_id: "c", t_start_s: 30, t_end_s: 31, open: false, count: 1 },
      { emitter_id: "b", t_start_s: 5, t_end_s: 6, open: false, count: 1 },
    ],
    emitters: [
      { id: "a", state: "candidate", f_center_hz: 105.59e6, bandwidth_hz: 90e3, known_status: "unknown", explanations: [] },
      { id: "b", state: "candidate", f_center_hz: 92.15e6, bandwidth_hz: 12e3, known_status: "unknown", explanations: [{ label: "FM" }] },
      { id: "c", state: "confirmed", f_center_hz: 98e6, bandwidth_hz: 180e3, known_status: "known", explanations: [] },
    ],
  };
  const it = unknownItems(r);
  assert.deepEqual(it.map((i) => i.hz), [92.15e6, 105.59e6]);
  assert.match(it[0].tag, /burst ×3/);
  assert.match(it[1].tag, /on air/);
});

test("strongest, quiet-but-active and survey runs come from the backend's own figures", () => {
  assert.equal(strongestItem({ found: false }).length, 0);
  assert.equal(strongestItem({ found: true, f_center_hz: 101.3e6, max_db: -71.2 })[0].hz, 101.3e6);
  const sch = { poi: [
    { f_lo: 1e6, f_hi: 2e6, observed_cells: 0, observed_fraction: 0, poi: [{ p_poi: 0.9 }] },
    { f_lo: 88e6, f_hi: 108e6, observed_cells: 4, observed_fraction: 1, poi: [{ p_poi: 0.4 }] },
  ] } as unknown as SchedulerResponse;
  assert.equal(quietItems(sch).length, 1, "an unobserved band is never suggested as active");
  const s = surveyItems({ window: { t0_s: 0, t1_s: 100 }, grid: { cells: 5, f_lo_hz: 100e6, f_cell_hz: 1e6 },
    any: { cells: [{ state: "unobserved" }, { state: "observed" }, { state: "observed" }, { state: "unobserved" }, { state: "excluded" }] } });
  assert.deepEqual(s.map((i) => i.title), ["101.000 MHz – 103.000 MHz", "104.000 MHz – 105.000 MHz"]);
  assert.deepEqual(s[0].time, { t0: 0, t1: 100 });
  assert.deepEqual(groupItems([...s, ...strongestItem({ found: true, f_center_hz: 1e8 })]).map((g) => g.group), ["strongest", "surveys"]);
  assert.match(peekLine([]), /nothing to suggest/);
});

test("thin client: the drawer source reaches no device route and only GETs", () => {
  const src = readFileSync("src/app/chrome/explore-drawer.ts", "utf8");
  assert.doesNotMatch(src, /\.post\(|\.put\(|\.del\(|\/api\/(device|retune|control)/);
  assert.match(src, /requestGoto/);
});
