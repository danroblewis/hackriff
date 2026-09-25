// T-994: outputs on the map. The Outputs dock bar is retired; a box with an open output (Listen,
// stream-out, decode, recording) draws an ACTIVE halo distinct from `selected` plus a corner badge
// naming the output kinds, with an audio level pulse — and the state comes from the backend's
// open-output records, never from a click. Each claim is stated against the degenerate
// implementation that would pass without it:
//
//  1. **Backend records, not intent.** An audio Listen whose server header has not arrived
//     (`opening`), a refused or ended one, an `ended` pipeline and an inactive recording are NOT
//     active; a running pipeline / active recording against an emitter IS, with no client action.
//  2. **The halo is geometry through the same mapping**: drawn OUTSIDE the box's own outline (so it
//     cannot be mistaken for the heavier `selected` outline), only on an active box, thickening with
//     the server-reported level, and still a stroke.
//  3. **The badge** is laid out per frame at the box's top-right corner, one glyph per kind, the
//     level carried as `--lvl`, and the feature's button names the outputs in words.
//  4. **The surface wires both** from the one activity map, and right-click on a detection's box
//     reaches its menu through the same polygon pick a click uses.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { boxActivity, levelFrac, type ServedPipeline, type ServedRecording } from "../src/app/dock/activity";
import type { OutputEntry } from "../src/app/state";
import {
  ACTIVE_GAP_CSS_PX, ACTIVE_MARK, ACTIVE_PULSE_CSS_PX, ACTIVE_RING_CSS_PX, CONFIRMED_STROKE_PX, SELECTED_EXTRA_CSS_PX,
  activeRingCssPx, markQuads, signalMarkBoxes,
} from "../src/surface/marks";
import { quadSizePx, type OverlayQuad } from "../src/surface/minimap";
import { PinLayer, detectionPins, isUnexplained, layoutPanePins, type PinRow } from "../src/surface/pins";
import type { PaneRect } from "../src/surface/surface";

const audio = (over: Partial<OutputEntry> = {}): OutputEntry => ({
  id: "l1", kind: "audio", label: "", sub: "", state: "live", tcpTarget: null, muted: false,
  levelDbfs: -50, recordsPerS: null, emitterId: "e1", pipelineId: null, message: null, ...over,
});

// ---- 1. the activity model ----

test("audio is active only once the SERVER's header arrived: opening, refused and ended are not", () => {
  for (const state of ["opening", "refused", "ended"] as const) {
    assert.equal(boxActivity([audio({ state })], [], []).size, 0, `${state} is not an open output`);
  }
  const a = boxActivity([audio()], [], []).get("e1");
  assert.deepEqual(a?.kinds, ["audio"]);
  assert.equal(a?.level, levelFrac(-50), "the level is the server-reported dBFS, scaled");
  assert.equal(boxActivity([audio({ emitterId: null })], [], []).size, 0, "a band Listen has no box to badge");
});

test("decode = a RUNNING pipeline against the emitter; rec = an ACTIVE recording; ended ones drop", () => {
  const pipes: ServedPipeline[] = [{ id: "p1", state: "running", emitter_id: "e2" }, { id: "p2", state: "ended", emitter_id: "e3" }, { id: "p3", state: "running", emitter_id: null }];
  const recs: ServedRecording[] = [{ id: "r1", active: true, emitter_id: "e2" }, { id: "r2", active: false, emitter_id: "e4" }];
  const m = boxActivity([], pipes, recs);
  assert.deepEqual([...m.keys()], ["e2"], "no client action anywhere: the records alone light e2");
  assert.deepEqual(m.get("e2")?.kinds, ["decode", "rec"]);
  assert.equal(m.get("e2")?.level, null, "no audio, no level");
  assert.equal(boxActivity([], [{ id: "p1", state: "ended", emitter_id: "e2" }], []).size, 0, "a decode that finished loses its badge");
});

test("stream = a streamed pipeline output this page holds, only while that pipeline runs; kinds in one order", () => {
  const rec: OutputEntry = audio({ id: "o1", kind: "records", emitterId: null, pipelineId: "p1" });
  const run = [{ id: "p1", state: "running", emitter_id: "e1" }];
  assert.deepEqual(boxActivity([rec, audio()], run, [{ id: "r", active: true, emitter_id: "e1" }]).get("e1")?.kinds, ["audio", "stream", "decode", "rec"]);
  assert.equal(boxActivity([rec], [{ id: "p1", state: "ended", emitter_id: "e1" }], []).size, 0);
});

test("levelFrac: -80..-20 dBFS onto 0..1, clamped; no level is null", () => {
  assert.equal(levelFrac(null), null);
  assert.equal(levelFrac(Number.NaN), null);
  assert.equal(levelFrac(-80), 0);
  assert.equal(levelFrac(-20), 1);
  assert.equal(levelFrac(0), 1);
  assert.equal(levelFrac(-50), 0.5);
});

// ---- 2. the halo ----

const S = 1e9, T0 = 1_700_000_000, EDGE = T0 * S;
const PANE = { f0Hz: 100e6, f1Hz: 102e6, t0Ns: (T0 - 20) * S, t1Ns: EDGE };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };
const GEN = { dpr: 1, generalizeBelowPx: 6 };
function row(id: string, over: Partial<PinRow> = {}): PinRow {
  return {
    id, state: "confirmed", f_center_hz: 101e6, bandwidth_hz: 200e3, f_lo_hz: 100.9e6, f_hi_hz: 101.1e6,
    family: "FM broadcast", explanations: [{ label: "FM broadcast" }],
    presence: { last_interval: { t_start_s: T0 - 12, t_end_s: T0 - 4, open: false } }, ...over,
  };
}
const extent = (qs: readonly OverlayQuad[]) => ({
  x0: Math.min(...qs.map((q) => q.clip[0])), y0: Math.min(...qs.map((q) => q.clip[1])),
  x1: Math.max(...qs.map((q) => q.clip[2])), y1: Math.max(...qs.map((q) => q.clip[3])),
});

test("the active halo is drawn OUTSIDE the outline, only on an active box, and is not the selected outline", () => {
  const act = new Map([["a", { level: null }]]);
  const q = markQuads(signalMarkBoxes([row("a"), row("b", { f_lo_hz: 101.5e6, f_hi_hz: 101.7e6 })], null, isUnexplained, act), EDGE, PANE, RECT, GEN);
  const halo = q.filter((x) => x.part === "active");
  assert.equal(halo.length, 4, "four edges of a ring");
  assert.ok(halo.every((x) => x.id === "a" && x.rgba === ACTIVE_MARK));
  assert.ok(!q.some((x) => x.id === "b" && x.part === "active"), "an inactive box carries no halo");
  const edge = extent(q.filter((x) => x.id === "a" && x.part === "edge")), ring = extent(halo);
  // Outside the box by one outline width + the gap (so it also clears a selected box's corner
  // handles, which sit one outline width out) + the ring, in device px (dpr 1).
  const px = ((edge.x0 - ring.x0) / 2) * RECT.w;
  assert.ok(Math.abs(px - (CONFIRMED_STROKE_PX + ACTIVE_GAP_CSS_PX + ACTIVE_RING_CSS_PX)) < 1e-6, `ring starts ${px} px left of the box`);
  assert.ok(ring.x1 > edge.x1 && ring.y0 < edge.y0 && ring.y1 > edge.y1);
  // Selected is a heavier outline + handles ON the box — no halo; active + selected draws both.
  const sel = markQuads(signalMarkBoxes([row("a")], "a", isUnexplained), EDGE, PANE, RECT, GEN);
  assert.ok(!sel.some((x) => x.part === "active"), "selected never draws the active halo");
  assert.ok(sel.some((x) => x.part === "handle"));
  const both = markQuads(signalMarkBoxes([row("a")], "a", isUnexplained, act), EDGE, PANE, RECT, GEN);
  assert.ok(both.some((x) => x.part === "active") && both.some((x) => x.part === "handle"));
  const bothRing = extent(both.filter((x) => x.part === "active")), bothEdge = extent(both.filter((x) => x.part === "edge"));
  const gap = ((bothEdge.x0 - bothRing.x0) / 2) * RECT.w;
  assert.ok(Math.abs(gap - (CONFIRMED_STROKE_PX + SELECTED_EXTRA_CSS_PX + ACTIVE_GAP_CSS_PX + ACTIVE_RING_CSS_PX)) < 1e-6,
    `clear of the heavier selected outline and its handles too (${gap} px)`);
  const handles = extent(both.filter((x) => x.part === "handle"));
  assert.ok(bothRing.x0 < handles.x0 && bothRing.x1 > handles.x1, "the ring runs outside the corner handles");
});

test("the level pulse: the halo thickens with the server-reported level, and stays a stroke", () => {
  const th = (level: number | null) => {
    const q = markQuads(signalMarkBoxes([row("a")], null, isUnexplained, new Map([["a", { level }]])), EDGE, PANE, RECT, GEN)
      .filter((x) => x.part === "active");
    return Math.min(...q.map((x) => Math.min(quadSizePx(x, RECT).wPx, quadSizePx(x, RECT).hPx)));
  };
  assert.ok(Math.abs(th(null) - ACTIVE_RING_CSS_PX) < 1e-6);
  assert.ok(Math.abs(th(0) - ACTIVE_RING_CSS_PX) < 1e-6);
  assert.ok(Math.abs(th(1) - (ACTIVE_RING_CSS_PX + ACTIVE_PULSE_CSS_PX)) < 1e-6);
  assert.ok(th(0.5) > th(0) && th(0.5) < th(1));
  assert.equal(activeRingCssPx(7), ACTIVE_RING_CSS_PX + ACTIVE_PULSE_CSS_PX, "clamped");
  assert.ok(th(1) <= 5 + 1e-6, "a stroke, never a wash");
});

test("a generalized (symbol) feature carries the halo around its symbol", () => {
  // 1 s x 1 kHz in a 20 s x 2 MHz pane: under 6 px both ways, so a symbol.
  const tiny = row("t", { f_lo_hz: 101e6, f_hi_hz: 101.001e6, presence: { last_interval: { t_start_s: T0 - 6, t_end_s: T0 - 5.9, open: false } } });
  const q = markQuads(signalMarkBoxes([tiny], null, isUnexplained, new Map([["t", { level: null }]])), EDGE, PANE, RECT, GEN);
  assert.ok(q.some((x) => x.part === "symbol"));
  const ring = extent(q.filter((x) => x.part === "active")), sym = extent(q.filter((x) => x.part === "symbol"));
  assert.ok(ring.x0 < sym.x0 && ring.x1 > sym.x1 && ring.y0 < sym.y0 && ring.y1 > sym.y1, "around the symbol");
});

// ---- 3. the badge ----

class FakeEl {
  children: FakeEl[] = [];
  hidden = false;
  className = "";
  type = "";
  title = "";
  textContent: string | null = "";
  dataset: Record<string, string> = {};
  attrs: Record<string, string> = {};
  removed = false;
  vars: Record<string, string> = {};
  style: Record<string, unknown> = {
    setProperty: (k: string, v: string) => { this.vars[k] = v; },
    getPropertyValue: (k: string) => this.vars[k] ?? "",
  };
  ownerDocument = { createElement: () => new FakeEl() };
  appendChild(c: FakeEl) { this.children.push(c); return c; }
  replaceChildren(...c: FakeEl[]) { this.children = c; }
  remove() { this.removed = true; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener() {}
  focus() {}
}

test("the badge: per frame at the box's top-right, one glyph per kind, --lvl pulse, named on the button; gone when the output closes", () => {
  const root = new FakeEl();
  const layer = new PinLayer(root as unknown as HTMLElement);
  const layout = [layoutPanePins(detectionPins([row("a")]), "p1", PANE, RECT, 500, 1, EDGE)];
  const placed = layout[0].placed[0];
  assert.ok(placed.drawn, "a box, not a symbol, at this scale");
  layer.update(layout, null, null, new Map([["a", { kinds: ["audio", "decode"] as const, level: 0.5 }]]));
  const badge = layer.badges.get("p1|a") as unknown as FakeEl;
  assert.ok(badge, "an active box has a badge");
  assert.equal(badge.dataset.kinds, "audio decode");
  assert.deepEqual(badge.children.map((c) => [c.className, c.textContent]), [["k-audio", "♪"], ["k-decode", "01"]]);
  assert.equal(badge.vars["--lvl"], "0.50", "the audio level drives the pulse");
  assert.equal(badge.style.transform, `translate(${(placed.drawn!.x1 - 2).toFixed(1)}px, ${(placed.drawn!.y0 + 2).toFixed(1)}px) translateX(-100%)`,
    "inside the top-right corner — the newest edge, which for a live signal is the pane's top");
  const btn = layer.element("p1", "a") as unknown as FakeEl;
  assert.match(btn.getAttribute("aria-label")!, / · active: listening, decoding$/);
  assert.match(btn.className, / active$/);
  // Next frame, the output closed (the records say so): badge and words go.
  layer.update(layout, null, null, new Map());
  assert.ok(badge.removed);
  assert.equal(layer.badges.size, 0);
  assert.doesNotMatch(btn.getAttribute("aria-label")!, /active/);
  assert.doesNotMatch(btn.className, /active/);
});

// ---- 4. the surface wires it ----

test("the surface draws the halo and the badge from ONE activity map, and right-click picks the box like a click does", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /signalMarkBoxes\(rows, focusId, isUnexplained, activityNow\(\)\)/, "the detections layer reads it per frame");
  assert.match(src, /pinLayer\.update\([^;]*activityNow\(\)\)/, "the badge layer reads the same map on the same frame");
  assert.match(src, /boxActivity\(s\.outputs, s\.servedOutputs\.pipelines, s\.servedOutputs\.recordings\)/, "from the backend's records");
  const ctxHandler = src.slice(src.indexOf("onContext: (p, e) => {"), src.indexOf("onContext: (p, e) => {") + 1200);
  assert.match(ctxHandler, /pinLayer\.pick\(/, "right-click / long-press hits a detection's box through the polygon pick");
  assert.match(ctxHandler, /openSignalMenu\(ctx, row/);
});
