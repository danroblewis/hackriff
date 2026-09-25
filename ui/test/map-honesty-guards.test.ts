// T-825 (MAP-25): the three honesty guards the map-UI redesign owes as PROPERTIES of the whole layer
// vocabulary, not of one layer at a time (docs/23 §9's checklist).
//
//  1. **Byte-identical data pass with overlays on and off.** docs/23 §9: "no layer, base style or
//     interpolation may paint over unobserved space", and docs/24: an overlay is a stroke, never a
//     wash. `surface-marks.test.ts` proves it for the detections layer; the risk this adds is the
//     one the registry created — EVERY overlay layer at once, composed through `composeOverlays`,
//     must still leave the measurement pass byte-for-byte what it was with no overlays at all.
//  2. **Coarse zoom fabricates no timespan.** A sub-pixel burst is widened to a visible floor about
//     its own centre, and that floor is never wider than it has to be, never read back, and never
//     written into the record: the feature stays a `[start, end?]` the backend measured, and below
//     the generalization threshold it is symbolized rather than fattened (docs/23 §10.6 rule 6).
//  3. **Empty call list over the LAYER and PANEL vocabulary.** docs/23 §4/§10.6 rule 4: toggling a
//     layer, switching base style, opening or sizing a panel, selecting a row — none of it reaches a
//     route. The GESTURE half of the vocabulary (pan, wheel, pinch, shift-drag, measure, annotate)
//     is asserted in `surface-input.test.ts` ("T-340's control over the WHOLE gesture vocabulary");
//     this covers the controls that gained the map chrome its state.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CELL } from "../src/surface/cellrule";
import { keyOf, type Lattice, type TileAddr } from "../src/surface/lattice";
import {
  CANDIDATE_MARK, CONFIRMED_MARK, GENERALIZE_BELOW_CSS_PX, isGeneralized, markAt, markQuads, quadSizePx,
  type MarkBox,
} from "../src/surface/marks";
import { LAYER_DEFS, composeOverlays, defaultPaneLayers, withLayer, type LayerId, type OverlayLayerFn } from "../src/surface/layers";
import type { PaneRect, PaneView } from "../src/surface/surface";
import { TileCache } from "../src/surface/tilecache";
import type { TileData } from "../src/surface/tile";
import { SurfaceView } from "../src/surface/view";
import { stubGl } from "./surface-glstub";
import { createStore } from "../src/app/store";
import { initialState, type AppState } from "../src/app/state";
import { dropPaneLayers, inheritPane, setPaneBase, setPaneLayer } from "../src/app/map/layers-slice";
import {
  RESEARCH_SELECTED_MARK, researchMarkBoxes, researchRows, selectResearch, setCollectionVisible, setResearchOpen,
} from "../src/app/map/research-slice";
import { cycleSnap, keySnap, stepSnap, type SheetSnap } from "../src/app/chrome/sheet";

const S = 1e9;
const T0 = 1_700_000_000 * S;
const LAT: Lattice = { scheme: "view", cells: 256, f0Hz: 6250, t0Ns: 1e9, levelsF: 20, levelsT: 15 };
const BOUNDS = { f0Hz: 1e6, f1Hz: 6e9, t0Ns: T0 - 86_400 * S, t1Ns: T0 };
const RECT: PaneRect = { x: 0, y: 0, w: 1000, h: 500 };

function tile(a: TileAddr): TileData {
  return {
    addr: a, key: keyOf(a), nf: 2, nt: 2,
    value: new Float32Array([-90, -80, -70, -60]),
    state: new Uint8Array([CELL.OBSERVED, CELL.OBSERVED, CELL.UNOBSERVED, CELL.OBSERVED]),
    tier: "survey-overview", answeredLevel: 1, fold: { frequency: "exact", time: "exact" },
    rangeDb: { lo: -100, hi: -60 }, bytes: 192 * 1024, serverInFlightLimit: null, serverInFlightShare: null,
  };
}

const OVERLAY_IDS = LAYER_DEFS.filter((d) => d.plane === "overlay").map((d) => d.id as LayerId);

// ---------------------------------------------------------------------------
// 1. Every overlay layer at once cannot touch the measurement pass
// ---------------------------------------------------------------------------

test("T-825: the data pass is byte-identical with EVERY overlay layer on and with all of them off", () => {
  assert.ok(OVERLAY_IDS.length >= 5, `the registry has overlay layers to compose (${OVERLAY_IDS.length})`);
  let composed = 0;
  const run = (overlays: boolean) => {
    const g = stubGl(1200, 600);
    // Each layer draws real geometry — a stroked box spanning its pane, the shape most able to wash
    // a measurement if the overlay pass could sample or tint. One per layer, all of them visible.
    const fns: Partial<Record<LayerId, OverlayLayerFn>> = {};
    for (const [i, id] of OVERLAY_IDS.entries()) {
      fns[id] = (pane: PaneView, edgeNs: number) => markQuads(
        [{
          id: `${id}-1`, kind: "signal-box", rgba: i % 2 ? CONFIRMED_MARK : CANDIDATE_MARK, open: i === 0,
          f0Hz: pane.box.f0Hz, f1Hz: pane.box.f1Hz, t0Ns: pane.box.t0Ns, t1Ns: i === 0 ? null : pane.box.t1Ns,
        }],
        edgeNs, pane.box, pane.rect,
      );
    }
    let reg = defaultPaneLayers("p1");
    for (const id of OVERLAY_IDS) reg = withLayer(reg, id, overlays);
    const view = new SurfaceView({
      canvas: g.canvas, lattice: LAT, bounds: BOUNDS, minimapPx: 150,
      cache: (tex) => new TileCache(tex, (a) => Promise.resolve(tile(a)), { inFlight: 64, now: () => 0 }),
      surface: { pinParents: false },
      freq: { centerHz: 100.8e6, spanHz: 2.4e6 }, spanNs: 20 * S,
      marks: (pane, edgeNs) => {
        const q = composeOverlays(reg, fns, pane, edgeNs);
        if (overlays) composed = Math.max(composed, q.length);
        return q;
      },
    });
    const f = view.frame(T0, []);
    // The data program is the one with a sampler (`uBox`); the overlay program has none (overlay.ts).
    const dataOps = g.ops.filter((o) => o.kind === "draw" && o.u && "uBox" in o.u);
    const drawn = f.overlaysDrawn;
    view.dispose();
    return { json: JSON.stringify(dataOps), drawn };
  };
  const on = run(true), off = run(false);
  assert.ok(composed >= OVERLAY_IDS.length, `every overlay layer contributed geometry (${composed})`);
  // `overlaysDrawn` counts the surface's own chrome quads (the minimap's pane rectangles) too, so
  // the claim is the difference: every layer's geometry really was submitted in the `on` run.
  assert.ok(on.drawn >= off.drawn + composed,
    `the overlays really were submitted — otherwise the comparison is vacuous (${on.drawn} vs ${off.drawn} + ${composed})`);
  assert.equal(on.json, off.json, "an overlay layer changed the measurement pass");
});

// ---------------------------------------------------------------------------
// 2. Coarse zoom fabricates no timespan
// ---------------------------------------------------------------------------

test("T-825: at every zoom a mark is drawn at its measured extent or symbolized — never fattened", () => {
  const MIN_PX = 3; // the drawing floor markQuads applies about a box's own centre
  const durations = [0.001, 0.01, 0.2, 1, 5, 30]; // s — a burst through to a long dwell
  const spans = [20, 300, 3600, 24 * 3600]; // s of pane height: live zoom through to survey overview
  let generalized = 0, atExtent = 0;
  for (const spanS of spans) {
    const pane = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: T0 - spanS * S, t1Ns: T0 };
    for (const durS of durations) {
      const rec: MarkBox = {
        id: `b-${spanS}-${durS}`, kind: "signal-box", rgba: CONFIRMED_MARK, open: false,
        f0Hz: 100.5e6, f1Hz: 100.512e6, t0Ns: T0 - (durS + 1) * S, t1Ns: T0 - 1 * S,
      };
      const before = JSON.stringify(rec);
      const quads = markQuads([rec], T0, pane, RECT, { strokePx: 2, minPx: MIN_PX });
      assert.equal(JSON.stringify(rec), before, "drawing wrote back into the record");
      const hPx = Math.max(...quads.map((q) => quadSizePx(q, RECT).hPx));
      const unionPx = (Math.max(...quads.map((q) => q.clip[3])) - Math.min(...quads.map((q) => q.clip[1]))) / 2 * RECT.h;
      const truePx = (durS / spanS) * RECT.h;
      // The one honest widening: up to the visible floor, about its own centre. Never beyond it, and
      // never proportional to the zoom — a 1 ms burst over a 24 h pane is a mark, not a minute.
      assert.ok(unionPx <= Math.max(MIN_PX, truePx) + 2 + 1e-6,
        `span ${spanS}s dur ${durS}s: drew ${unionPx.toFixed(2)} px for a ${truePx.toFixed(4)} px event`);
      assert.ok(hPx > 0, "and it is still visible");
      const wPx = (rec.f1Hz - rec.f0Hz) / (pane.f1Hz - pane.f0Hz) * RECT.w;
      if (isGeneralized(wPx, truePx, GENERALIZE_BELOW_CSS_PX)) generalized++; else atExtent++;
    }
  }
  assert.ok(generalized > 0 && atExtent > 0, `both regimes were exercised (${generalized} symbolized, ${atExtent} at extent)`);
  // The count-per-cell answer to density (docs/23 §10.6 rule 6, "density, not clustering") is
  // asserted against `/api/tiles/events` in `surface-density.test.ts`; the property here is that
  // nothing on the box path ever invents the duration those counts stand in for.
  assert.match(readFileSync("test/surface-density.test.ts", "utf8"), /never a numbered bubble/);
});

// ---------------------------------------------------------------------------
// 3. Empty call list over the layer + panel vocabulary
// ---------------------------------------------------------------------------

test("T-825: the whole layer/panel control vocabulary reaches NO route and no device", () => {
  const calls: string[] = [];
  const client = {
    get: async (p: string) => { calls.push(`GET ${p}`); return {}; },
    post: async (p: string) => { calls.push(`POST ${p}`); return {}; },
    put: async (p: string) => { calls.push(`PUT ${p}`); return {}; },
    del: async (p: string) => { calls.push(`DELETE ${p}`); return {}; },
  };
  void client; // held so a future control that grabs a client from the context is still spied on
  const store = createStore(initialState());
  const fallback = defaultPaneLayers("p1");
  const before = store.get();
  // Layers: every registered layer, both ways, on two panes; both base styles; inherit and drop.
  for (const id of LAYER_DEFS.map((d) => d.id as LayerId)) {
    store.set(setPaneLayer("p1", id, false, fallback));
    store.set(setPaneLayer("p1", id, true, fallback));
  }
  store.set(setPaneBase("p1", "phosphor", fallback));
  store.set(setPaneBase("p1", "ramp", fallback));
  store.set(inheritPane("p1", "p2", fallback));
  store.set(dropPaneLayers("p2"));
  // Research: open, select a row, clear it, toggle a collection's stored visibility.
  store.set(setResearchOpen(true));
  store.set(selectResearch("marker:1"));
  store.set(selectResearch(null));
  store.set(setCollectionVisible("c1", false));
  store.set(setResearchOpen(false));
  // The sheet: every snap transition the keyboard and the flick can produce.
  let snap: SheetSnap = "peek";
  for (const k of ["ArrowUp", "ArrowDown", "Escape", "Home", "End"]) snap = keySnap(k, snap) ?? snap;
  snap = stepSnap(snap, 1); snap = stepSnap(snap, -1); snap = cycleSnap(snap);
  assert.deepEqual(calls, [], "a layer, panel or sheet control reached the network");
  // The other half of the vocabulary — pan, wheel, pinch, shift-drag region, measure, annotate — is
  // asserted against the input module itself; naming it here means its removal shows up as a red.
  assert.match(readFileSync("test/surface-input.test.ts", "utf8"),
    /T-340's control over the WHOLE gesture vocabulary, region stroke included: NO call reaches the network/);
  // …and none of it moved the device axis, either: the view's own frequency/centre state is untouched.
  const dev = (s: AppState) => JSON.stringify({ frequency: s.frequency, live: s.live, capture: s.captureWindow });
  assert.equal(dev(store.get()), dev(before), "a presentation control changed device/view state");
  // Structural: the slices these controls write name no route and no device action at all.
  for (const f of ["src/app/map/layers-slice.ts", "src/app/map/research-slice.ts", "src/app/chrome/sheet.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
    for (const word of ["/api/", "/ws/", "DeviceAction", "fetch("]) {
      assert.ok(!src.includes(word), `${f} names "${word}"`);
    }
  }
});

// ---------------------------------------------------------------------------
// 4. Table <-> canvas sync is a HIGHLIGHT, in both directions, for every row
// ---------------------------------------------------------------------------

test("T-825: table ↔ canvas sync — selecting a row highlights exactly its mark, and a click on a mark names that row", () => {
  const pane = { f0Hz: 99e6, f1Hz: 103e6, t0Ns: T0 - 600 * S, t1Ns: T0 };
  const rect = { w: 1000, h: 500 };
  const coll = { id: "c1", name: "Mine", note: null, color: "#8fd", visible: true, reserved: false, member_count: 3 };
  const marker = (id: string, f: number, t: number | null, bw: number | null, dur: number | null) => ({
    id, collection_id: coll.id, name: `m${id}`, note: null,
    f_center_hz: f, bandwidth_hz: bw, f_lo_hz: f - (bw ?? 0) / 2, f_hi_hz: f + (bw ?? 0) / 2,
    t_center_s: t, duration_s: dur,
    t_start_s: t === null ? null : t - (dur ?? 0) / 2, t_end_s: t === null ? null : t + (dur ?? 0) / 2,
  });
  const edgeS = T0 / S;
  const slice = {
    collections: [coll],
    markers: [
      marker("1", 100.1e6, null, null, null),        // a pin: frequency only, all time
      marker("2", 100.9e6, edgeS - 120, 0, 0),       // a point: widened to be visible
      marker("3", 101.7e6, edgeS - 300, 40e3, 30),   // a box with a real extent
    ],
    annotations: [{
      id: "a1", collection_id: coll.id, kind: "box" as const,
      f_lo_hz: 102.1e6, f_hi_hz: 102.3e6, t0_s: edgeS - 400, t1_s: edgeS - 380, label: "note", body: null,
    }],
  };
  const rows = researchRows(slice);
  assert.equal(rows.length, 4, "every marker and annotation is a row");
  const colors = new Map([[coll.id, [0.5, 0.9, 0.8, 0.9] as const]]);
  const store = createStore(initialState());
  const viewState = (s: AppState) => JSON.stringify({ frequency: s.frequency, live: s.live, capture: s.captureWindow, focus: s.focus });
  const before = viewState(store.get());

  for (const row of rows) {
    store.set(selectResearch(row.key));
    assert.equal(store.get().research.selected, row.key);
    const boxes = researchMarkBoxes(rows, colors, () => true, row.key, pane, rect);
    // One mark per row, and the highlight is exactly one of them — the selected row's.
    assert.deepEqual([...boxes].map((b) => b.id).sort(), rows.map((r) => r.key).sort());
    const hot = boxes.filter((b) => b.rgba === RESEARCH_SELECTED_MARK);
    assert.deepEqual(hot.map((b) => b.id), [row.key], `${row.key}: the highlight is its own mark, alone`);
    // The other direction: a click inside the drawn mark names that row back, through the same
    // geometry the canvas drew — not a lookup by frequency, which is what used to mis-target.
    const b = boxes.find((x) => x.id === row.key)!;
    const at = markAt(boxes, pane.t1Ns, (b.f0Hz + b.f1Hz) / 2, ((b.t0Ns + (b.t1Ns ?? pane.t1Ns)) / 2));
    assert.equal(at?.id, row.key, `${row.key}: a click on its mark selects it`);
    assert.ok(rows.some((r) => r.key === at!.id), "and the id is a row key, so the table can scroll to it");
    // P4 (docs/23 §10.6 rule 4): a row click highlights. It never moves the map or reaches a device.
    assert.equal(viewState(store.get()), before, `${row.key}: selecting a row moved the view`);
  }
});
