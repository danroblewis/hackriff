// T-044 click-to-inspect: choosing the inventory emitter nearest a click.
import { test } from "node:test";
import assert from "node:assert/strict";
import { extentDistanceHz, inspectHalfWidthHz, nearestEntry } from "../src/inspect";

const row = (id: string, lo: number, hi: number, last = 0) => ({ id, f_lo_hz: lo, f_hi_hz: hi, bandwidth_hz: hi - lo, last_seen_s: last });

test("distance to an extent is zero inside it", () => {
  const r = row("a", 101.2e6, 101.4e6);
  assert.equal(extentDistanceHz(r, 101.3e6), 0);
  assert.equal(extentDistanceHz(r, 101.1e6), 100e3);
  assert.equal(extentDistanceHz(r, 101.45e6), 50e3);
});

test("the nearest emitter wins; ties prefer narrower, then more recent", () => {
  const station = row("station", 101.2e6, 101.4e6);
  const pilot = row("narrow", 101.29e6, 101.31e6);
  const far = row("far", 101.6e6, 101.7e6);
  assert.equal(nearestEntry([station, far], 101.3e6, 10e3)?.id, "station");
  assert.equal(nearestEntry([station, pilot], 101.3e6, 10e3)?.id, "narrow");
  assert.equal(nearestEntry([row("old", 1, 2, 5), row("new", 1, 2, 9)], 1.5, 1)?.id, "new");
  assert.equal(nearestEntry([far], 101.3e6, 10e3), null, "beyond the search width");
  assert.equal(nearestEntry([far, row("near", 101.31e6, 101.32e6)], 101.3e6, 20e3)?.id, "near");
  assert.equal(nearestEntry([], 1, 1), null);
});

test("search half-width scales with zoom and bin width", () => {
  assert.equal(inspectHalfWidthHz(2.4e6, 585.9375), 12e3);
  assert.equal(inspectHalfWidthHz(20e6, 4882.8125), 100e3);
  assert.equal(inspectHalfWidthHz(200e3, 585.9375), 5e3);
});
