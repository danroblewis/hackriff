// T-1008: the scan plan as a buttoned map overlay.
//
// Two halves, each asserted against what the user sees or what the client sends:
//  1. GEOMETRY (`surface/scanplan.ts`): the plan draws the SERVED steps — never a tiling of its own —
//     as strokes and a screen-door hatch (never a wash), over the pane's whole height, with the
//     dwelling step bright and covered steps faded, merged so a 3 000-step pass is a few quads.
//  2. CONTROLLER (`app/map/scan-overlay.ts`), over a fake DOM and a spy client: opening a plan is a
//     READ (`GET …windows=1` over the pane's window), a drag re-prices, Start posts exactly the plan
//     that was priced, the running plan's steps are the server's, and Stop comes from the same
//     button. A replay offers no scan and sends nothing.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  MIN_STEP_PX, dragRegion, scanEdgeAt, scanPlanQuads, stepStatus, stepsDrawn,
  type ScanOverlayModel, type ScanWindow,
} from "../src/surface/scanplan";
import { inkFraction } from "../src/surface/minimap";
import type { ScanState } from "../src/controls/model";

const BOX = { f0Hz: 90e6, f1Hz: 110e6, t0Ns: 0, t1Ns: 60e9 };
const RECT = { x: 0, y: 0, w: 1000, h: 500 };

/** Served-looking windows: `n` equal slices of [lo, hi] — built HERE as test data only. */
function windows(lo: number, hi: number, n: number): ScanWindow[] {
  const w = (hi - lo) / n;
  return Array.from({ length: n }, (_, i) => ({ step: i, lo_hz: lo + i * w, hi_hz: lo + (i + 1) * w, center_hz: lo + (i + 0.5) * w }));
}
const plan = (w: ScanWindow[] | null, over: Partial<ScanOverlayModel> = {}): ScanOverlayModel => ({
  state: "plan", loHz: 94e6, hiHz: 106e6, windows: w, dwellStep: null, nextStep: null, editable: true, ...over,
});
const xPx = (clipX: number) => ((clipX + 1) / 2) * RECT.w;

// ---------------------------------------------------------------------------
// 1. Geometry
// ---------------------------------------------------------------------------

test("the plan draws the served steps: hatch over their span, a line at each boundary, handles at the edges", () => {
  const w = windows(94e6, 106e6, 6);
  const q = scanPlanQuads(plan(w), BOX, RECT);
  assert.ok(q.length > 0);
  assert.ok(q.every((x) => x.kind === "scan-plan"), "every quad is the scan layer's own kind");
  const fills = q.filter((x) => x.part === "fill");
  assert.equal(fills.length, 1, "six pending steps merge into one hatch");
  assert.ok(Math.abs(xPx(fills[0].clip[0]) - 200) < 1e-6 && Math.abs(xPx(fills[0].clip[2]) - 800) < 1e-6,
    "the hatch spans exactly the served steps (94-106 MHz of 90-110 = 200..800 px)");
  // The whole pane height: a plan is a frequency programme, drawn over grey and data alike.
  assert.deepEqual([fills[0].clip[1], fills[0].clip[3]], [-1, 1]);
  // A fill is a screen-door hatch — off-pattern pixels are discarded, so it can never wash a cell.
  for (const f of fills) assert.ok(f.pattern?.mode === "hatch" && inkFraction(f) < 0.5, "a fill is a hatch, never a wash");
  // Step boundaries at the SERVED lo of steps 1..5 (dashed), and the two handles.
  const steps = q.filter((x) => x.id.startsWith("scan:step:"));
  assert.equal(steps.length, 5);
  for (const [i, s] of steps.entries()) {
    const centre = xPx((s.clip[0] + s.clip[2]) / 2);
    assert.ok(Math.abs(centre - (200 + (i + 1) * 100)) < 1, `boundary ${i + 1} at its served frequency (${centre})`);
  }
  assert.equal(q.filter((x) => x.part === "handle").length, 2, "both region edges are drawn as handles");
});

test("without served steps (being re-priced) the region alone is drawn — no step is invented", () => {
  const q = scanPlanQuads(plan(null), BOX, RECT);
  assert.equal(q.filter((x) => x.id.startsWith("scan:step:")).length, 0, "no step line without the server's steps");
  const fills = q.filter((x) => x.part === "fill");
  assert.equal(fills.length, 1);
  assert.equal(fills[0].id, "scan:region");
});

test("too many steps to read: no boundary lines, a bounded number of quads, and the panel is told", () => {
  const w = windows(90e6, 110e6, 3334);
  const m = plan(w, { loHz: 90e6, hiHz: 110e6, state: "running", editable: false, dwellStep: 1500, nextStep: 1501 });
  const q = scanPlanQuads(m, BOX, RECT);
  assert.equal(q.filter((x) => x.id.startsWith("scan:step:")).length, 0, `steps under ${MIN_STEP_PX} px are not lined`);
  assert.ok(q.length <= 10, `3 334 steps merge into a handful of quads (${q.length})`);
  assert.equal(stepsDrawn(m, BOX, RECT), false);
  assert.equal(stepsDrawn(plan(windows(94e6, 106e6, 6)), BOX, RECT), true);
  // The dwelling step is still visible: widened to a few px about its own centre.
  const dw = q.filter((x) => x.id === "scan:dwelling:1500" && x.part === "fill");
  assert.equal(dw.length, 1);
  assert.ok(xPx(dw[0].clip[2]) - xPx(dw[0].clip[0]) >= 3.9, "the dwelling step is never sub-pixel");
});

test("progress comes from the server's indices: covered steps fade, the dwelling one is brightest", () => {
  const w = windows(94e6, 106e6, 6);
  const m = plan(w, { state: "running", editable: false, dwellStep: 2, nextStep: 3 });
  assert.deepEqual(w.map((_, i) => stepStatus(m, i)), ["covered", "covered", "dwelling", "pending", "pending", "pending"]);
  const q = scanPlanQuads(m, BOX, RECT);
  const fill = (id: string) => q.find((x) => x.id === id && x.part === "fill")!;
  const covered = fill("scan:covered:0"), dwelling = fill("scan:dwelling:2"), pending = fill("scan:pending:3");
  assert.ok(covered.rgba[3] < pending.rgba[3] && pending.rgba[3] < dwelling.rgba[3], "covered < pending < dwelling in ink");
  assert.ok(inkFraction(covered) < inkFraction(pending) && inkFraction(pending) < inkFraction(dwelling),
    "covered steps are the sparsest hatch, so the coverage filling in beneath them reads");
  // A yielded sweep dwells nowhere; what it covered stays covered.
  const y = plan(w, { state: "yielded", editable: false, dwellStep: null, nextStep: 3 });
  assert.deepEqual(w.map((_, i) => stepStatus(y, i)), ["covered", "covered", "covered", "pending", "pending", "pending"]);
  // A plan (nothing committed) is all pending, whatever indices are lying around.
  assert.equal(stepStatus(plan(w, { dwellStep: 1, nextStep: 4 }), 0), "pending");
});

test("a plan outside the pane draws nothing; one cut by the pane draws only its visible part", () => {
  assert.deepEqual(scanPlanQuads(plan(windows(200e6, 210e6, 4), { loHz: 200e6, hiHz: 210e6 }), BOX, RECT), []);
  const q = scanPlanQuads(plan(windows(80e6, 100e6, 4), { loHz: 80e6, hiHz: 100e6 }), BOX, RECT);
  for (const x of q) assert.ok(x.clip[0] >= -1 && x.clip[2] <= 1, "nothing drawn outside the pane");
  assert.equal(q.filter((x) => x.id === "scan:lo").length, 0, "an edge off the pane is not pinned to its border");
  assert.equal(scanPlanQuads(null, BOX, RECT).length, 0);
});

test("the edges are grabbed within reach, only on an editable plan; a drag never crosses or collapses", () => {
  const m = plan(windows(94e6, 106e6, 6));
  assert.equal(scanEdgeAt(m, BOX, RECT, 203), "lo");
  assert.equal(scanEdgeAt(m, BOX, RECT, 795), "hi");
  assert.equal(scanEdgeAt(m, BOX, RECT, 500), null, "the middle of the plan is the map's, not a handle");
  assert.equal(scanEdgeAt({ ...m, editable: false }, BOX, RECT, 203), null, "a running sweep is not dragged");
  assert.deepEqual(dragRegion(94e6, 106e6, "hi", 100e6, 1e4), { loHz: 94e6, hiHz: 100e6 });
  assert.deepEqual(dragRegion(94e6, 106e6, "lo", 120e6, 1e4), { loHz: 106e6 - 1e4, hiHz: 106e6 });
  assert.deepEqual(dragRegion(94e6, 106e6, "hi", 10e6, 1e4), { loHz: 94e6, hiHz: 94e6 + 1e4 });
});

// ---------------------------------------------------------------------------
// 2. The controller, over a minimal fake DOM and a spy client
// ---------------------------------------------------------------------------

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: FakeEl[] = [];
  attrs: Record<string, string> = {};
  dataset: Record<string, string> = {};
  classes = new Set<string>();
  classList = {
    add: (c: string) => this.classes.add(c), remove: (c: string) => this.classes.delete(c),
    contains: (c: string) => this.classes.has(c),
    toggle: (c: string, on?: boolean) => { const v = on ?? !this.classes.has(c); if (v) this.classes.add(c); else this.classes.delete(c); return v; },
  };
  textContent = "";
  hidden = false;
  disabled = false;
  title = "";
  value = "";
  handlers: Record<string, Handler[]> = {};
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x === "string") this.textContent += x; else this.children.push(x); }
  setAttribute(k: string, v: string) { this.attrs[k] = v; if (k === "hidden") this.hidden = true; if (k === "value") this.value = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  fire(t: string) { for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {} }); }
  find(cls: string): FakeEl | undefined {
    for (const c of this.children) {
      if ((c.attrs.class ?? "").split(" ").includes(cls)) return c;
      const d = c.find(cls); if (d) return d;
    }
    return undefined;
  }
}

const SERVED = windows(94e6, 106e6, 6);
const BUDGET = { steps: 6, dwell_s: 1, pass_s: 6, revisit_s: 6, span_hz: 12e6, step_span_hz: 2e6, duty: 1 / 6, statement: "6 steps × 1.0 s" };
const PLAN = { f_lo_hz: 94e6, f_hi_hz: 106e6, dwell_s: 1, recommended_dwell: false, sample_rate_hz: 2.4e6, steps: 6, warnings: [], step: "fine" as const };
const idle: ScanState = { state: "idle", available: true, unavailable_reason: null, device_id: "mock:1", plan: null, budget: null, progress: null, yielded: null };
const running = (dwell: number | null, next: number): ScanState => ({
  ...idle, state: "running", plan: PLAN, budget: BUDGET,
  progress: { step: next, steps: 6, pass: 0, steps_done: next, center_hz: dwell === null ? null : SERVED[dwell].center_hz, started_s: 1000, step_started_s: 1001, next_step_in_s: 0.5, dwell_step: dwell },
});

async function withController(fn: (h: {
  ctl: import("../src/app/map/scan-overlay").ScanController; calls: { m: string; path: string; body?: unknown }[]; toasts: string[];
  mod: typeof import("../src/app/map/scan-overlay");
}) => Promise<void>) {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window, fetch: g.fetch };
  g.document = { createElement: (t: string) => new FakeEl(t) };
  g.window = { addEventListener: () => {} };
  g.fetch = () => Promise.reject(new Error("the controller must go through its client"));
  const calls: { m: string; path: string; body?: unknown }[] = [];
  const toasts: string[] = [];
  const client = {
    get: async (path: string) => {
      calls.push({ m: "GET", path });
      const u = new URL(path, "http://x");
      if (u.searchParams.has("f_lo_hz")) {
        const lo = Number(u.searchParams.get("f_lo_hz")), hi = Number(u.searchParams.get("f_hi_hz"));
        return { scan: idle, proposed: { plan: { ...PLAN, f_lo_hz: lo, f_hi_hz: hi, windows: windows(lo, hi, 3) }, budget: BUDGET } };
      }
      return { scan: { ...running(0, 1), plan: { ...PLAN, windows: SERVED } }, proposed: null };
    },
    post: async (path: string, body?: unknown) => {
      calls.push({ m: "POST", path, body });
      if (path.endsWith("/stop")) return { scan: idle };
      return { scan: { ...running(null, 0), plan: { ...PLAN, windows: SERVED } }, device: { id: "mock:1" } };
    },
  };
  try {
    const mod = await import("../src/app/map/scan-overlay");
    const ctl = new mod.ScanController({
      client: client as never, paneWindow: () => ({ f0Hz: 94e6, f1Hz: 106e6 }), toast: (t) => toasts.push(t),
    });
    await fn({ ctl, calls, toasts, mod });
  } finally {
    g.document = saved.document; g.window = saved.window; g.fetch = saved.fetch;
  }
}
const settle = () => new Promise((r) => setTimeout(r, 0));

test("CONTROLLER: opening a plan is a READ over the pane's window, and the overlay draws the SERVED steps", async () => {
  await withController(async ({ ctl, calls, mod }) => {
    ctl.update(idle);
    const btn = ctl.button as unknown as FakeEl;
    assert.equal(btn.disabled, false);
    btn.fire("click");
    await settle();
    assert.deepEqual(calls.map((c) => c.m), ["GET"], "opening and pricing reach no device route");
    assert.equal(calls[0].path, mod.scanPriceRequest({ loHz: 94e6, hiHz: 106e6 }, mod.MAP_SCAN_DWELL_S, "fine"));
    const m = ctl.model()!;
    assert.equal(m.state, "plan");
    assert.equal(m.editable, true);
    assert.deepEqual(m.windows, windows(94e6, 106e6, 3), "the drawn steps are the answer's windows, verbatim");
    assert.equal((ctl.panel as unknown as FakeEl).hidden, false, "the plan panel is shown");
    const pub = JSON.parse((ctl.panel as unknown as FakeEl).dataset.plan);
    assert.equal(pub.windows.length, 3);
    assert.equal(pub.device_id, "mock:1", "the panel names the device that would do it");
  });
});

test("CONTROLLER: dragging an edge draws no steps until the server re-prices the new region", async () => {
  await withController(async ({ ctl, calls, mod }) => {
    ctl.update(idle);
    (ctl.button as unknown as FakeEl).fire("click");
    await settle();
    ctl.dragTo("hi", 100e6);
    assert.equal(ctl.model()!.windows, null, "mid-drag, the old steps are not stretched over the new region");
    assert.equal(ctl.model()!.hiHz, 100e6);
    ctl.endDrag();
    await settle();
    assert.equal(calls.at(-1)!.path, mod.scanPriceRequest({ loHz: 94e6, hiHz: 100e6 }, mod.MAP_SCAN_DWELL_S, "fine"));
    assert.deepEqual(ctl.model()!.windows, windows(94e6, 100e6, 3));
    assert.ok(calls.every((c) => c.m === "GET"), "no device route while planning");
  });
});

test("CONTROLLER: Start posts exactly the priced plan; progress and Stop come through the same button", async () => {
  await withController(async ({ ctl, calls, mod, toasts }) => {
    ctl.update(idle);
    (ctl.button as unknown as FakeEl).fire("click");
    await settle();
    const start = (ctl.panel as unknown as FakeEl).find("map-scan-go")!;
    assert.equal(start.disabled, false, "a priced plan can be started");
    start.fire("click");
    await settle();
    const post = calls.find((c) => c.m === "POST")!;
    assert.equal(post.path, mod.SCAN_START_PATH);
    assert.deepEqual(post.body, mod.scanStartBody({ loHz: 94e6, hiHz: 106e6 }, mod.MAP_SCAN_DWELL_S, "fine"));
    assert.match(toasts.at(-1)!, /Scanning 6 steps on mock:1/);
    // The running plan's steps are the start answer's — and a poll with the same plan re-reads none.
    ctl.update(running(2, 3));
    const m = ctl.model()!;
    assert.equal(m.state, "running");
    assert.equal(m.editable, false);
    assert.deepEqual(m.windows, SERVED);
    assert.equal(m.dwellStep, 2);
    assert.equal(m.nextStep, 3);
    const gets = calls.filter((c) => c.path === mod.SCAN_WINDOWS_REQUEST).length;
    ctl.update(running(3, 4));
    await settle();
    assert.equal(calls.filter((c) => c.path === mod.SCAN_WINDOWS_REQUEST).length, gets, "no per-poll windows read");
    // Stop: the SAME button.
    const btn = ctl.button as unknown as FakeEl;
    assert.equal(btn.find("map-scan-label")!.textContent, "Stop");
    btn.fire("click");
    await settle();
    assert.equal(calls.at(-1)!.path, mod.SCAN_STOP_PATH);
    assert.equal(ctl.model(), null, "a stopped sweep leaves nothing on the map");
    assert.equal(btn.find("map-scan-label")!.textContent, "Scan");
  });
});

test("CONTROLLER: a sweep started elsewhere is drawn from its own served steps, read once", async () => {
  await withController(async ({ ctl, calls, mod }) => {
    ctl.update(running(0, 1));
    await settle();
    ctl.update(running(1, 2));
    await settle();
    assert.equal(calls.filter((c) => c.path === mod.SCAN_WINDOWS_REQUEST).length, 1);
    assert.deepEqual(ctl.model()!.windows, SERVED);
    assert.equal((ctl.panel as unknown as FakeEl).hidden, false, "its progress panel is shown");
  });
});

test("CONTROLLER: a replay offers no scan, says why, and sends nothing", async () => {
  await withController(async ({ ctl, calls }) => {
    ctl.update(null);
    const btn = ctl.button as unknown as FakeEl;
    assert.equal(btn.disabled, true);
    assert.match(btn.title, /not_live/);
    ctl.openPlan();
    await settle();
    assert.deepEqual(calls, []);
    assert.equal(ctl.model(), null);
  });
});
