// T-051 control panel: frequency entry, steps, zoom/pan and pooling maths, request building (token
// only in the header, never in a mutating URL), error-code reactions, replay disabling (against a
// state body captured from `hk serve --replay`), bookmarks.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as ax from "../src/axis";
import { bookmarkFromClick, bookmarkFromSelection, jumpPlan } from "../src/controls/bookmarks";
import { ControlClient, ControlError, buildRequest, errorFrom, reactionTo } from "../src/controls/client";
import { STEPS, clampToRanges, formatFrequency, parseFrequency, shiftCenter, stepHz } from "../src/controls/freq";
import {
  type ControlState, basebandFilterOptions, classLabel, fftSizeOptions, gainControls, panelModel, rateOptions,
  rowRateOptions, segmentNotice, windowLabel,
} from "../src/controls/model";
import replayState from "./control_state_replay.json";

const near = (a: number, b: number, tol: number, msg = "") => assert.ok(Math.abs(a - b) <= tol, `${msg}: |${a} − ${b}| > ${tol}`);
const FM = { centerHz: 100.8e6, bandwidthHz: 2.4e6, bins: 4096 };

test("frequency entry parses units, bare MHz, exponents and relative shifts", () => {
  assert.equal(parseFrequency("101.3M"), 101.3e6);
  assert.equal(parseFrequency("433.92 MHz"), 433.92e6);
  assert.equal(parseFrequency("433.92mhz"), 433.92e6);
  assert.equal(parseFrequency("1.09 GHz"), 1.09e9);
  assert.equal(parseFrequency("7k"), 7e3);
  assert.equal(parseFrequency("12.5 kHz"), 12.5e3);
  assert.equal(parseFrequency("2.4e6"), 2.4e6);
  assert.equal(parseFrequency("915000000"), 915e6);
  assert.equal(parseFrequency("100 Hz"), 100, "explicit Hz is never promoted to MHz");
  assert.equal(parseFrequency("101.3"), 101.3e6, "bare small numbers are MHz");
  assert.equal(parseFrequency(" 1090 "), 1090e6);
  assert.equal(parseFrequency("+25k", 101.3e6), 101.325e6);
  assert.equal(parseFrequency("-1M", 101.3e6), 100.3e6);
  assert.equal(parseFrequency("+500", 101.3e6), 101.3005e6, "relative bare numbers are Hz");
  for (const bad of ["", "abc", "101.3 X", "1,090", "M", "-5", "0", "1..2M", "+25k"]) assert.equal(parseFrequency(bad), null, bad);
  assert.equal(formatFrequency(101.3e6), "101.3 MHz");
  assert.equal(formatFrequency(433.92e6), "433.92 MHz");
  assert.equal(formatFrequency(1.09e9), "1.09 GHz");
  assert.equal(formatFrequency(12.5e3), "12.5 kHz");
  assert.equal(parseFrequency(formatFrequency(433.921875e6)), 433.921875e6, "round-trips");
});

test("step and shift: fixed steps snap to their grid, span steps walk the band, ranges clamp", () => {
  const step = (label: string) => STEPS.find((s) => s.label === label)!.step;
  assert.equal(shiftCenter(101.33e6, step("100 kHz"), 2.4e6, 1), 101.4e6);
  assert.equal(shiftCenter(101.33e6, step("100 kHz"), 2.4e6, -1), 101.3e6);
  assert.equal(shiftCenter(101.3e6, step("100 kHz"), 2.4e6, 1), 101.4e6, "on the grid: one full step");
  assert.equal(shiftCenter(101.3e6, step("100 kHz"), 2.4e6, -1), 101.2e6);
  assert.equal(shiftCenter(145.0e6, step("12.5 kHz"), 2e6, 1), 145.0125e6);
  assert.equal(shiftCenter(433.92e6, step("6.25 kHz"), 2e6, -1), 433.91875e6, "off-grid: down to the grid point below");
  assert.equal(shiftCenter(433.91875e6, step("6.25 kHz"), 2e6, 1), 433.925e6);
  assert.equal(shiftCenter(100.8e6, step("½ span"), 2.4e6, 1), 102e6);
  assert.equal(shiftCenter(100.8e6, step("span"), 2.4e6, -1), 98.4e6);
  assert.equal(stepHz(step("½ span"), 20e6), 10e6);
  const hackrf: [number, number][] = [[1e6, 6e9]];
  assert.equal(shiftCenter(1.5e6, step("span"), 2e6, -1, hackrf), 1e6, "clamped at the bottom");
  assert.equal(shiftCenter(5.999e9, step("5 MHz"), 20e6, 1, hackrf), 6e9, "clamped at the top");
  assert.equal(clampToRanges(3e9, [[1e6, 1e9], [2e9, 2.5e9]]), 2.5e9, "nearest range");
  assert.equal(clampToRanges(42, undefined), 42);
});

test("zoom about the pointer and pan keep the frequency under the finger; pans report overflow", () => {
  const full = ax.fullView(FM), df = ax.binWidthHz(FM);
  const z = ax.zoomAt(FM, full, 0.25, 4);
  near(ax.fracToHz(z, 0.25), ax.fracToHz(full, 0.25), 1e-3, "pointer frequency fixed");
  near(z.hiHz - z.loHz, 0.6e6, 1e-3, "4× zoom");
  assert.deepEqual(ax.zoomAt(FM, z, 0.5, 1e-6), full, "zooming out clamps to the band");
  near(ax.zoomAt(FM, full, 0.5, 1e9).hiHz - ax.zoomAt(FM, full, 0.5, 1e9).loHz, 8 * df, 1e-6, "minimum 8 bins");
  assert.ok(ax.wheelFactor(-100) > 1 && ax.wheelFactor(100) < 1, "scroll up zooms in");
  near(ax.wheelFactor(3, 1), ax.wheelFactor(48, 0), 1e-12, "line mode = 16 px");
  near(ax.wheelFactor(-1e6) , Math.exp(0.8), 1e-12, "clamped per event");

  const v = { loHz: 101e6, hiHz: 101.2e6 };
  assert.deepEqual(ax.panView(FM, v, 50e3), { view: { loHz: 101.05e6, hiHz: 101.25e6 }, overflowHz: 0 });
  const past = ax.panView(FM, v, 1.5e6);
  near(past.view.hiHz, full.hiHz, 1e-6, "clamped to the top edge");
  near(past.view.hiHz - past.view.loHz, 0.2e6, 1e-6, "width kept");
  near(past.overflowHz, 102.7e6 - full.hiHz, 1e-6, "overflow above");
  near(ax.panRetuneCenter(past.view, past.overflowHz), 102.6e6, 1e-6, "retune centre shows the requested view");
  const below = ax.panView(FM, full, -300e3);
  assert.deepEqual(below.view, full);
  near(below.overflowHz, -300e3, 1e-6, "overflow below");
});

test("pooling window is centred on the pixel (T-045 review nit)", () => {
  // 4096 texels on 1024 px: each pixel covers exactly 4 texels, starting at 4·px.
  const W = 1024, N = 4096, pxU = 1 / W;
  for (const px of [0, 1, 511, 1023]) {
    const u = (px + 0.5) / W;
    assert.deepEqual(ax.poolWindow(u, pxU, N), [4 * px, 4], `pixel ${px}`);
  }
  // 4096 on 1000 px: the window straddles the pixel's footprint symmetrically (old code: [x0, x0+5) from its left edge).
  const u = (500 + 0.5) / 1000, [x0, n] = ax.poolWindow(u, 1 / 1000, N);
  const a = (u - 0.0005) * N, b = (u + 0.0005) * N;
  assert.ok(x0 <= a && x0 + n >= b && x0 > a - 1 && x0 + n < b + 1, `[${x0}, ${x0 + n}) covers [${a}, ${b}] tightly`);
  assert.deepEqual(ax.poolWindow(0.5, 1 / 8192, N), [2048, 1], "zoomed in: the texel under the pixel centre");
  assert.deepEqual(ax.poolWindow(0.5, 1, 1 << 20), [(1 << 19) - 32, 64], "capped at 64 taps, centred");
  assert.deepEqual(ax.poolWindow(0.0001, 1 / 100, N), [0, 21], "clamped at the left edge");
  // The peak bin of the FM golden stays under the pixel drawn at its frequency.
  const peakBin = 2901, full = ax.fullView(FM), pxAt = Math.floor(ax.hzToFrac(full, ax.binHz(FM, peakBin)) * 1236);
  const [p0, pn] = ax.poolWindow((pxAt + 0.5) / 1236, 1 / 1236, N);
  assert.ok(peakBin >= p0 && peakBin < p0 + pn, "peak bin pooled by its own pixel");
});

test("requests: bearer header on every call, JSON bodies on mutations, token never in a URL", async () => {
  const tok = "0123456789abcdef0123456789abcdef";
  const post = buildRequest("POST", "/api/control/center", tok, { center_hz: 101.3e6 });
  assert.equal(post.url, "/api/control/center");
  assert.equal(post.init.method, "POST");
  assert.equal(post.init.headers.Authorization, `Bearer ${tok}`);
  assert.equal(post.init.headers["Content-Type"], "application/json");
  assert.equal(post.init.body, JSON.stringify({ center_hz: 101.3e6 }));
  assert.ok(!post.url.includes(tok));
  assert.throws(() => buildRequest("POST", `/api/control/pause?token=${tok}`, tok), /never goes in a fetch URL/);
  assert.throws(() => buildRequest("DELETE", "/api/bookmarks/x?a=1&token=t", tok), /never goes in a fetch URL/);
  assert.throws(() => buildRequest("POST", "https://evil.example/api/control/center", tok, {}), /origin-relative/);
  assert.throws(() => buildRequest("POST", "//evil.example/api", tok, {}), /origin-relative/);
  assert.throws(() => buildRequest("GET", "/api/control/state", tok, {}), /no body/);

  const seen: { url: string; init: RequestInit }[] = [];
  const reply = (status: number, body: unknown) => ({ ok: status < 300, status, statusText: "", json: async () => body });
  const client = new ControlClient(tok, async (url, init) => {
    seen.push({ url, init });
    return url.includes("rate") ? reply(409, { error: "device settings apply to a live source", code: "not_live" }) : reply(200, { ok: true });
  });
  await client.get("/api/control/state");
  await client.post("/api/control/pause");
  await client.post("/api/control/gains", { gains: { lna: 24 } });
  await client.put("/api/bookmarks/abc", { name: "x" });
  await client.del("/api/bookmarks/abc");
  await assert.rejects(client.post("/api/control/rate", { sample_rate_hz: 2e6 }), (e: unknown) =>
    e instanceof ControlError && e.status === 409 && e.code === "not_live");
  for (const { url, init } of seen) {
    const h = init.headers as Record<string, string>;
    assert.equal(h.Authorization, `Bearer ${tok}`, `${init.method} ${url}`);
    assert.ok(!url.includes("token") && !url.includes(tok), `${init.method} ${url}`);
  }
  const byUrl = (m: string, u: string) => seen.find((s) => s.init.method === m && s.url === u)!.init;
  assert.equal(byUrl("GET", "/api/control/state").body, undefined);
  assert.equal(byUrl("POST", "/api/control/pause").body, "{}", "empty mutations send {}");
  assert.equal(byUrl("POST", "/api/control/gains").body, '{"gains":{"lna":24}}');
  assert.equal(byUrl("DELETE", "/api/bookmarks/abc").body, undefined);
});

test("error codes map to reactions", () => {
  const r = (status: number, body: unknown) => reactionTo(errorFrom(status, body)).reaction;
  assert.equal(r(401, { error: "unauthorized" }), "reauth");
  assert.equal(r(409, { code: "not_live", error: "x" }), "not-live");
  assert.equal(r(409, { code: "conflict", error: "a re-plumb is in progress" }), "busy");
  assert.equal(r(409, { code: "refused", error: "recording refused under restricted-paging" }), "refused");
  assert.equal(r(504, { code: "timeout", error: "x" }), "pending");
  assert.equal(r(409, { code: "finished", error: "x" }), "finished");
  assert.equal(r(400, { code: "invalid", error: "x" }), "field");
  assert.equal(r(400, { code: "out_of_range", error: "x" }), "field");
  assert.equal(r(501, { code: "unsupported", error: "no bias tee" }), "unsupported");
  assert.equal(r(503, { code: "unavailable", error: "no audit log" }), "error");
  assert.equal(reactionTo(new TypeError("Failed to fetch")).reaction, "offline");
  assert.equal(errorFrom(403, "").code, "forbidden");
  assert.equal(errorFrom(502, null, "Bad Gateway").message, "Bad Gateway");
  assert.match(reactionTo(errorFrom(409, { code: "refused", error: "class forbids content" })).message, /refused: class forbids content/);
});

const HACKRF_CAPS = {
  driver: "hackrf-one", kind: "hardware", controllable: true, frequency_ranges_hz: [[1e6, 6e9]],
  sample_rates_hz: { min: 2e6, max: 20e6 },
  gain_stages: [{ name: "lna", min_db: 0, max_db: 40, step_db: 8 }, { name: "vga", min_db: 0, max_db: 62, step_db: 2 }, { name: "amp", min_db: 0, max_db: 11, step_db: 11 }],
  bias_tee: true, baseband_filter: { values_hz: [1.75e6, 2.5e6, 3.5e6, 5e6, 5.5e6, 6e6, 7e6, 8e6, 9e6, 10e6, 12e6, 14e6, 15e6, 20e6, 24e6, 28e6] },
  adc_bits: 8, tx_capable_hardware: true,
} as const;

function liveState(over: Partial<{ class: string; permitted: boolean; replumbing: boolean; audit: boolean; finished: boolean; recording: boolean }> = {}): ControlState {
  const s = structuredClone(replayState) as unknown as ControlState;
  s.live = true;
  s.device = structuredClone(HACKRF_CAPS) as unknown as ControlState["device"];
  s.tuning = { center_hz: 100.8e6, sample_rate_hz: 2.4e6, gains: { lna: 32, vga: 30, amp: 11 }, bias_tee: "off", baseband_filter_hz: 7e6 };
  s.audit = over.audit ?? true;
  const run = s.run!;
  run.live = true;
  run.content_class = over.class ?? "unrestricted";
  run.content_permitted = over.permitted ?? true;
  run.replumbing = !!over.replumbing;
  run.finished = !!over.finished;
  run.recording.active = !!over.recording;
  return s;
}

test("replay (captured hk serve --replay state): device controls off with not_live, display and record on", () => {
  const s = replayState as unknown as ControlState;
  assert.equal(s.live, false);
  assert.equal(s.device, null);
  assert.equal(s.transmit.available, false);
  const m = panelModel(s);
  assert.equal(m.device.enabled, false);
  assert.match(m.device.reason, /^not_live/);
  assert.equal(m.display.enabled, true);
  assert.equal(m.record.enabled, s.run!.content_permitted);
  assert.deepEqual(m.gains, []);
  assert.equal(m.biasTee.available, false);
  assert.equal(m.basebandFilter.available, false);
  assert.deepEqual(m.rates, [s.run!.sample_rate_hz], "only the recording's rate");
  assert.equal(m.classText, classLabel(s.run!.content_class, s.run!.content_permitted));
  assert.deepEqual(m.limits, s.display_limits, "T-067: read from the state body, not hard-coded");
});

test("T-325: bias tee has three states, and unknown is never shown as off", () => {
  const off = liveState();
  assert.deepEqual(panelModel(off).biasTee, { available: true, on: false, unknown: false });

  const on = liveState();
  on.tuning!.bias_tee = "on";
  assert.deepEqual(panelModel(on).biasTee, { available: true, on: true, unknown: false });

  // "nothing reported" is its own state: not on, and explicitly not off either.
  const unknown = liveState();
  unknown.tuning!.bias_tee = "unknown";
  const m = panelModel(unknown);
  assert.deepEqual(m.biasTee, { available: true, on: false, unknown: true });
  assert.notDeepEqual(m.biasTee, panelModel(off).biasTee, "unknown differs from off");
});

test("live state: HackRF gains, rates, bias tee; busy, gated, finished and no-audit gates", () => {
  const m = panelModel(liveState());
  assert.equal(m.device.enabled, true);
  assert.deepEqual(m.gains.map((g) => [g.label, g.value, g.step, g.toggle]), [["LNA", 32, 8, false], ["VGA", 30, 2, false], ["RF amp", 11, 11, true]]);
  assert.deepEqual(m.rates, [2e6, 2.4e6, 4e6, 5e6, 8e6, 10e6, 12.5e6, 16e6, 20e6]);
  assert.deepEqual(m.biasTee, { available: true, on: false, unknown: false });
  assert.equal(m.basebandFilter.available, true);
  assert.equal(m.basebandFilter.value, 7e6);
  assert.deepEqual(m.basebandFilter.options, HACKRF_CAPS.baseband_filter.values_hz);
  assert.deepEqual(m.frequencyRanges, [[1e6, 6e9]]);

  const busy = panelModel(liveState({ replumbing: true }));
  assert.equal(busy.device.enabled, false);
  assert.equal(busy.busy, true);
  assert.match(busy.device.reason, /30 s/);
  assert.equal(panelModel(liveState(), true).device.enabled, false, "a request in flight blocks a second one");

  const paging = panelModel(liveState({ class: "restricted-paging", permitted: false }));
  assert.equal(paging.classText, "restricted-paging: metadata only");
  assert.equal(paging.gated, true);
  assert.equal(paging.record.enabled, false);
  assert.match(paging.record.reason, /restricted-paging/);
  assert.equal(paging.device.enabled, true, "retuning out of a restricted band stays possible");

  const rec = panelModel(liveState({ recording: true }));
  assert.equal(rec.record.active, true);
  assert.equal(rec.record.enabled, false, "no second start while recording");

  const done = panelModel(liveState({ finished: true }));
  assert.equal(done.device.enabled || done.display.enabled, false);
  const noAudit = panelModel(liveState({ audit: false }));
  assert.equal(noAudit.device.enabled || noAudit.display.enabled || noAudit.record.enabled, false);

  const generic = gainControls({ ...HACKRF_CAPS, gain_stages: [{ name: "IFGR", min_db: -1, max_db: 49.6, step_db: 0 }, { name: "TUNER", min_db: 0, max_db: 50, step_db: 1 }] } as never, null);
  assert.deepEqual(generic.map((g) => [g.label, g.value, g.step, g.toggle]), [["IFGR", -1, 0.5, false], ["Tuner", 0, 1, false]],
    "unknown stages keep the device's own name; known ones get a label");
  assert.deepEqual(rateOptions({ ...HACKRF_CAPS, sample_rates_hz: { values: [2.048e6, 1.024e6] } } as never, 3.2e6), [1.024e6, 2.048e6, 3.2e6]);
  assert.deepEqual(rateOptions(null, undefined), []);
});

test("segment changes after a legal-class retune are announced", () => {
  const run = liveState({ class: "restricted-paging", permitted: false }).run!;
  run.segment = 2;
  assert.equal(segmentNotice(null, run), null);
  assert.equal(segmentNotice({ segment: 2, content_class: "restricted-paging" }, run), null);
  assert.equal(segmentNotice({ segment: 1, content_class: "unrestricted" }, run),
    "re-plumbed: segment 1 → 2; class unrestricted → restricted-paging: metadata only");
  assert.match(segmentNotice({ segment: 1, content_class: "restricted-paging" }, run)!, /class unchanged/);
});

test("bookmarks: payloads from a click or a selection; jump zooms inside the band, retunes outside", () => {
  assert.deepEqual(bookmarkFromClick(101.3e6), { kind: "marker", name: "101.3 MHz", f_center_hz: 101.3e6 });
  assert.equal(bookmarkFromClick(1, "  my   marker ").name, "my marker");
  assert.equal(bookmarkFromClick(1, "x".repeat(300)).name.length, 120);
  assert.deepEqual(bookmarkFromSelection({ name: "Region 1", f_lo: 101.2e6, f_hi: 101.4e6 }),
    { kind: "bookmark", name: "Region 1", f_center_hz: 101.3e6, bandwidth_hz: 200e3 });
  assert.deepEqual(jumpPlan(FM, { f_center_hz: 101.3e6, bandwidth_hz: 200e3 }, false), { kind: "zoom", loHz: 101.0e6, hiHz: 101.6e6 });
  assert.deepEqual(jumpPlan(FM, { f_center_hz: 101.3e6, bandwidth_hz: null }, false), { kind: "zoom", loHz: 101.275e6, hiHz: 101.325e6 });
  assert.deepEqual(jumpPlan(FM, { f_center_hz: 433.92e6, bandwidth_hz: null }, true), { kind: "retune", centerHz: 433.92e6 });
  assert.deepEqual(jumpPlan(FM, { f_center_hz: 433.92e6, bandwidth_hz: null }, false), { kind: "outside" });
  assert.deepEqual(jumpPlan(null, { f_center_hz: 433.92e6, bandwidth_hz: null }, true), { kind: "retune", centerHz: 433.92e6 });
});

test("T-067: display limits, window and baseband filter options are derived, not hard-coded", () => {
  const limits = { fft_size_min: 256, fft_size_max: 4096, averaging_max: 16, rows_per_s_min: 1, rows_per_s_max: 30, windows: ["hann", "flat-top"] };
  assert.deepEqual(fftSizeOptions(limits), [256, 512, 1024, 2048, 4096]);
  assert.deepEqual(rowRateOptions(limits), [1, 2, 5, 10, 25, 30], "standard rates clamped into range, max always included");
  assert.equal(windowLabel("hann"), "Hann");
  assert.equal(windowLabel("blackman-harris"), "Blackman-Harris");
  assert.equal(windowLabel("flat-top"), "Flat-Top");

  assert.deepEqual(basebandFilterOptions(null, undefined), []);
  assert.deepEqual(basebandFilterOptions({ values_hz: [7e6, 1.75e6, 20e6] }, undefined), [1.75e6, 7e6, 20e6], "sorted");
  assert.deepEqual(
    basebandFilterOptions({ values_hz: [7e6, 20e6] }, 9.5e6),
    [7e6, 9.5e6, 20e6],
    "the current value is included even if not offered by the device"
  );
  const cont = basebandFilterOptions({ min_hz: 1e6, max_hz: 8e6 }, undefined);
  assert.equal(cont.length, 9);
  assert.equal(cont[0], 1e6);
  assert.equal(cont[cont.length - 1], 8e6);
  assert.ok(cont.every((v, i) => i === 0 || v > cont[i - 1]!), "increasing");
});
