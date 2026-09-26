// T-1009: **a measurement box's own actions.** The Measure tool (T-822) draws an arbitrary box; the
// user asked that such a box be able to start a scan on a chosen SDR. This asserts the menu the box
// gets — per-radio Scan and Record IQ, plus Save as marker — as data (no DOM: the menu component
// itself is T-192's and is tested there).
import { test } from "node:test";
import assert from "node:assert/strict";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import type { AppContext } from "../src/app/context";
import type { MarkMeasurement } from "../src/surface/marks";
import type { MeasureView } from "../src/app/explore/measure";
import {
  MEASURE_COLLECTION_NAME, collectionForMarkers, markerName, measurementMenuItems, menuDevices,
  type MeasureMenuHost,
} from "../src/app/menu/measure-actions";
import { deviceLabels, type AttachedDevice } from "../src/surface/panedevice";

const T0 = 1_700_000_000;
const BOX: MarkMeasurement = { id: "m-1", f_lo_hz: 97e6, f_hi_hz: 99e6, t0_s: T0, t1_s: T0 + 3 };
const VIEW: MeasureView = { center_hz: 98e6, span_hz: 20e6, t_capture: [T0, T0 + 30], tier: "live-iq" };
const ATTACHED: AttachedDevice[] = [
  { id: "mock:hackrf", driver: "hackrf-one", centerHz: 98e6, sampleRateHz: 2.4e6 },
  { id: "mock:rtl", driver: "rtl-sdr", centerHz: 433e6, sampleRateHz: 2.048e6 },
];

interface Call { m: string; path: string; body?: unknown }

function harness(devices: AttachedDevice[], answers: Record<string, unknown> = {}) {
  const calls: Call[] = [];
  const client = {
    get: async (path: string) => { calls.push({ m: "GET", path }); return answers[path] ?? { collections: [] }; },
    post: async (path: string, body?: unknown) => { calls.push({ m: "POST", path, body }); return { id: "made-1", recording: { id: "r1", samples: 42 } }; },
  };
  const ctx = { client, store: createStore(initialState()) } as unknown as AppContext;
  const scans: { region: { loHz: number; hiHz: number }; deviceId: string | null }[] = [];
  const host: MeasureMenuHost = {
    devices: menuDevices(devices, deviceLabels(devices)),
    view: VIEW,
    openScan: (region, deviceId) => scans.push({ region, deviceId }),
    prompt: () => "my region",
  };
  return { ctx, calls, host, scans, client };
}
const settle = () => new Promise((r) => setTimeout(r, 0));
const toastOf = (ctx: AppContext) => ctx.store.get().toast.text;

test("with two radios the menu offers each one by name, for Scan and for Record IQ", () => {
  const { ctx, host, scans } = harness(ATTACHED);
  const items = measurementMenuItems(ctx, BOX, host);
  const scanItems = items.filter((i) => i.id.startsWith("scan"));
  const recItems = items.filter((i) => i.id.startsWith("record-iq"));
  assert.deepEqual(scanItems.map((i) => i.id), ["scan:mock:hackrf", "scan:mock:rtl"]);
  assert.deepEqual(recItems.map((i) => i.id), ["record-iq:mock:hackrf", "record-iq:mock:rtl"]);
  for (const i of [...scanItems, ...recItems]) assert.ok(/HackRF|RTL/.test(i.label), `${i.label} names its radio`);
  assert.ok(items.some((i) => i.id === "save-marker"));

  // Choosing the RTL-SDR opens the plan bounded by the BOX, on that radio — the ticket's own case.
  scanItems[1].onSelect();
  assert.deepEqual(scans, [{ region: { loHz: 97e6, hiHz: 99e6 }, deviceId: "mock:rtl" }]);
});

test("with one radio the selector is omitted — and the item still says which radio it is about", () => {
  const { ctx, host, scans } = harness([ATTACHED[0]]);
  const items = measurementMenuItems(ctx, BOX, host);
  const scan = items.find((i) => i.id === "scan")!;
  assert.equal(scan.label, "Scan this region");
  assert.match(scan.hint!, /HackRF/);
  scan.onSelect();
  assert.deepEqual(scans, [{ region: { loHz: 97e6, hiHz: 99e6 }, deviceId: null }]);
});

test("on a replay the device items are present and disabled WITH the reason, never silently dropped", () => {
  const { ctx, host } = harness([]);
  const items = measurementMenuItems(ctx, BOX, host);
  for (const id of ["scan", "record-iq"]) {
    const it = items.find((i) => i.id === id)!;
    assert.equal(it.disabled, true, `${id} is disabled on a replay`);
    assert.match(it.hint!, /no front end/);
  }
  assert.equal(items.find((i) => i.id === "save-marker")!.disabled, false, "a marker needs no radio");
});

test("Record IQ posts the box's own window and band off the chosen radio's ring, and reports the outcome", async () => {
  const { ctx, calls, host } = harness(ATTACHED);
  measurementMenuItems(ctx, BOX, host).find((i) => i.id === "record-iq:mock:rtl")!.onSelect();
  await settle();
  const c = calls.find((x) => x.path === "/api/iqbuffer/clip")!;
  assert.deepEqual(c.body, {
    t0: BOX.t0_s, t1: BOX.t1_s,
    band: { f_lo: BOX.f_lo_hz, f_hi: BOX.f_hi_hz },
    label: "measured 97.000–99.000 MHz",
    device_id: "mock:rtl",
  });
  assert.match(toastOf(ctx), /Recorded 97\.000–99\.000 MHz from RTL/);
});

test("Save as marker files a time–frequency BOX in a real collection, and keeps the extent that was drawn", async () => {
  const existing = { collections: [{ id: "c-reserved", name: "Bookmarks", reserved: true }, { id: "c-mine", name: "Survey" }] };
  const { ctx, calls, host } = harness(ATTACHED, { "/api/collections": existing });
  measurementMenuItems(ctx, BOX, host).find((i) => i.id === "save-marker")!.onSelect();
  await settle();
  await settle();
  const post = calls.find((x) => x.m === "POST")!;
  assert.equal(post.path, "/api/collections/c-mine/markers", "never the reserved Bookmarks collection");
  assert.deepEqual(post.body, {
    name: "my region",
    f_center_hz: 98e6, bandwidth_hz: 2e6,
    t_center_s: T0 + 1.5, duration_s: 3,
    view: VIEW,
  });
  assert.match(toastOf(ctx), /Saved marker: my region/);
});

test("with no collection but the reserved one, a marker collection is created rather than a bookmark written", async () => {
  const only = { collections: [{ id: "c-reserved", name: "Bookmarks", reserved: true }] };
  const { calls, client } = harness(ATTACHED, { "/api/collections": only });
  const id = await collectionForMarkers(client as never);
  assert.equal(id, "made-1");
  assert.deepEqual(calls.at(-1), { m: "POST", path: "/api/collections", body: { name: MEASURE_COLLECTION_NAME } });
});

test("the offered marker name states the place, in the readout's own words", () => {
  assert.equal(markerName(BOX), "97.000–99.000 MHz · 3.0 s");
});
