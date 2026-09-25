// T-806 (MAP-06): the per-pane layer registry and the layers menu's two axes (docs/24 §4, §13).
//
// The claims, each against the degenerate implementation that would otherwise pass:
//  1. **Defaults are docs/24 §13.4's**, and Candidate/unknown detections are visible by default —
//     a registry that shipped with detections off would hide the interesting part.
//  2. **Planes paint in a fixed order and `z` never crosses a plane**: no toggle can move a stroke
//     beneath a measurement's colour, and a stored preference cannot reorder planes.
//  3. **The overlays reach the ONE marks hook**: only visible `overlay` layers, in ascending z; a
//     function handed in for a `data` layer is ignored, so it cannot sneak geometry into the stroke
//     pass. With every overlay hidden the hook returns nothing — the byte-identical guard's input.
//  4. **Per pane**: toggling pane A changes pane A only; a split inherits by value and diverges.
//  5. **Toggling a layer reaches no route** (the spy-client empty-call-list rule).
//  6. **Storage absent or garbage renders the defaults**, never throws.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  COLLECTION_Z, LAYER_DEFS, PLANE_ORDER, composeOverlays, defaultPaneLayers, inheritPaneLayers, isLayerVisible, layerDef,
  loadPaneLayers, paintOrder, parsePaneLayers, savePaneLayers, serializePaneLayers, withBase, withLayer,
  type LayerId, type OverlayLayerFn,
} from "../src/surface/layers";
import type { OverlayQuad } from "../src/surface/minimap";
import type { PaneView } from "../src/surface/surface";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { inheritPane, paneLayersOf, setPaneBase, setPaneLayer, dropPaneLayers } from "../src/app/map/layers-slice";
import type { AppContext } from "../src/app/context";

const quad = (id: string): OverlayQuad => ({ clip: [-1, -1, 1, 1], rgba: [1, 1, 1, 1], kind: "signal-box", id } as OverlayQuad);
const pane = (id: string) => ({ id } as unknown as PaneView);

test("MAP-06: defaults follow docs/24 §13.4 — detections (incl. Candidate/unknown) on, suggestions off", () => {
  const r = defaultPaneLayers("p1");
  for (const id of ["base", "coverage", "detections", "rules", "pins"] as LayerId[]) assert.equal(isLayerVisible(r, id), true, id);
  for (const id of ["tier", "artifacts", "priors", "research"] as LayerId[]) assert.equal(isLayerVisible(r, id), false, id);
  assert.equal(r.base, "ramp");
  // z within overlay: rules 10 → detections 20 → research 30 → collection 40 → artifacts 50 → priors 60
  const z = (id: LayerId) => layerDef(id)!.z;
  assert.ok(z("rules") < z("detections") && z("detections") < z("research") && z("research") < z("collection:x")
    && z("collection:x") < z("artifacts") && z("artifacts") < z("priors"), "overlay z order");
});

test("MAP-06: planes paint data → overlay → dom, z orders only within a plane, and no toggle moves a plane", () => {
  let r = defaultPaneLayers("p1");
  r = withLayer(withLayer(r, "priors", true), "tier", true);
  const order = paintOrder(r);
  const ranks = order.map((l) => PLANE_ORDER.indexOf(l.plane));
  assert.deepEqual(ranks, [...ranks].sort((a, b) => a - b), "a layer painted out of plane order");
  // A dom layer with the lowest z still paints after every overlay: z never crosses a plane.
  assert.equal(layerDef("pins")!.z < layerDef("priors")!.z, true);
  assert.ok(order.findIndex((l) => l.id === "pins") > order.findIndex((l) => l.id === "priors"));
  // Every layer's plane is its def's, whatever was toggled.
  for (const l of r.layers) assert.equal(l.plane, layerDef(l.id)!.plane, l.id);
  // `base` is the style axis, not a visibility: it refuses to hide.
  assert.equal(isLayerVisible(withLayer(r, "base", false), "base"), true);
});

test("MAP-06: the overlays compose into ONE marks list — visible overlay layers only, ascending z", () => {
  const calls: string[] = [];
  const fn = (id: string): OverlayLayerFn => () => { calls.push(id); return [quad(id)]; };
  const fns: Partial<Record<LayerId, OverlayLayerFn>> = {
    priors: fn("priors"), detections: fn("detections"), rules: fn("rules"),
    // A data-plane layer handed a function must not reach the stroke pass.
    coverage: fn("coverage"),
  };
  let r = defaultPaneLayers("p1");
  assert.deepEqual(composeOverlays(r, fns, pane("p1"), 0).map((q) => q.id), ["rules", "detections"]);
  r = withLayer(r, "priors", true);
  assert.deepEqual(composeOverlays(r, fns, pane("p1"), 0).map((q) => q.id), ["rules", "detections", "priors"]);
  assert.ok(!calls.includes("coverage"), "a data layer's function was called by the overlay composer");
  // A hidden layer's function is not even called (no work, no side effects for hidden layers).
  calls.length = 0;
  r = withLayer(r, "detections", false);
  composeOverlays(r, fns, pane("p1"), 0);
  assert.ok(!calls.includes("detections"));
  // Everything off: the hook contributes nothing — the "overlays off" input of the byte-identical guard.
  for (const d of LAYER_DEFS) r = withLayer(r, d.id, false);
  assert.deepEqual(composeOverlays(r, fns, pane("p1"), 0), []);
});

test("MAP-06: registries are per pane — a toggle on one pane leaves the other's picture alone; a split inherits by value", () => {
  const store = createStore(initialState());
  const tpl = defaultPaneLayers("template");
  const fns: Partial<Record<LayerId, OverlayLayerFn>> = { detections: (p) => [quad(`det@${p.id}`)], rules: (p) => [quad(`rules@${p.id}`)] };
  const draw = (id: string) => composeOverlays(paneLayersOf(store.get(), id, tpl), fns, pane(id), 0).map((q) => q.id);

  store.set(inheritPane("p1", "p2", tpl));
  assert.deepEqual(draw("p1"), ["rules@p1", "det@p1"]);
  assert.deepEqual(draw("p2"), ["rules@p2", "det@p2"]);

  store.set(setPaneLayer("p1", "detections", false, tpl));
  assert.deepEqual(draw("p1"), ["rules@p1"], "pane 1's toggle did not take");
  assert.deepEqual(draw("p2"), ["rules@p2", "det@p2"], "pane 1's toggle changed pane 2");

  store.set(setPaneBase("p2", "phosphor", tpl));
  assert.equal(paneLayersOf(store.get(), "p2", tpl).base, "phosphor");
  assert.equal(paneLayersOf(store.get(), "p1", tpl).base, "ramp", "pane 2's base style changed pane 1");

  // Split from pane 1: pane 3 starts with pane 1's registry, then diverges without touching it.
  store.set(inheritPane("p1", "p3", tpl));
  assert.deepEqual(draw("p3"), ["rules@p3"]);
  store.set(setPaneLayer("p3", "detections", true, tpl));
  assert.deepEqual(draw("p3"), ["rules@p3", "det@p3"]);
  assert.deepEqual(draw("p1"), ["rules@p1"], "the child's toggle leaked into its parent (inherited by reference)");

  const a = defaultPaneLayers("a");
  const b = inheritPaneLayers(a, "b");
  assert.notEqual(a.layers, b.layers);
  assert.equal(withBase(a, "ramp"), a, "a no-op edit must not allocate (the frame compares by identity)");

  store.set(dropPaneLayers("p3"));
  assert.equal("p3" in store.get().layers, false);
});

test("MAP-06: toggling layers and base style reaches NO route", () => {
  const fetched: unknown[] = [];
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  g.fetch = (...a: unknown[]) => { fetched.push(a); return Promise.reject(new Error("a layer toggle reached the network")); };
  const calls: unknown[] = [];
  const store = createStore(initialState());
  const client = {
    post: (path: string) => { calls.push(["POST", path]); return Promise.resolve({}); },
    get: (path: string) => { calls.push(["GET", path]); return Promise.resolve({}); },
  } as unknown as AppContext["client"];
  void client;
  try {
    const tpl = defaultPaneLayers("template");
    for (const d of LAYER_DEFS) { store.set(setPaneLayer("p1", d.id, false, tpl)); store.set(setPaneLayer("p1", d.id, true, tpl)); }
    store.set(setPaneBase("p1", "phosphor", tpl));
    store.set(setPaneBase("p1", "ramp", tpl));
    store.set(inheritPane("p1", "p2", tpl));
    assert.equal(paneLayersOf(store.get(), "p1", tpl).base, "ramp", "the toggles did not act");
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(fetched, []);
  assert.deepEqual(calls, []);
  // And structurally: neither the registry nor its slice names a route or a client.
  for (const f of ["src/surface/layers.ts", "src/app/map/layers-slice.ts"]) {
    const src = readFileSync(f, "utf8").replace(/\/\/.*$/gm, "");
    for (const banned of ["fetch(", "/api/", "client.", "DeviceAction"]) assert.ok(!src.includes(banned), `${f} reaches for ${banned}`);
  }
});

test("MAP-06: persistence — per pane id, ignores stored planes, and renders defaults with storage absent or garbage", () => {
  let r = withBase(withLayer(defaultPaneLayers("pane2"), "priors", true), "phosphor");
  r = withLayer(r, "detections", false);
  const raw = serializePaneLayers(null, r);
  const back = parsePaneLayers(raw, "pane2")!;
  assert.equal(back.base, "phosphor");
  assert.equal(isLayerVisible(back, "priors"), true);
  assert.equal(isLayerVisible(back, "detections"), false);
  // Another pane's edit is not this pane's default: pane 1 has nothing stored.
  assert.equal(parsePaneLayers(raw, "pane1"), null, "pane 2's stored layers leaked to pane 1");
  const both = parsePaneLayers(serializePaneLayers(raw, defaultPaneLayers("pane1")), "pane2")!;
  assert.equal(both.base, "phosphor", "saving pane 1 dropped pane 2's entry");
  for (const junk of [null, "", "{", "[]", '{"pane1":"x"}']) assert.equal(parsePaneLayers(junk, "pane1"), null, `junk ${junk}`);
  assert.deepEqual(parsePaneLayers('{"pane1":{"base":"neon","visible":{"nope":true,"rules":"yes"}}}', "pane1"), defaultPaneLayers("pane1"));
  // A stored plane/z is not read: planes come from the defs, so storage cannot reorder them.
  const sneaky = parsePaneLayers('{"z":{"visible":{"priors":true},"layers":[{"id":"priors","plane":"data","z":-1}]}}', "z")!;
  assert.equal(sneaky.layers.find((l) => l.id === "priors")!.plane, "overlay");

  const g = globalThis as { localStorage?: unknown };
  const real = g.localStorage;
  try {
    g.localStorage = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); } };
    assert.equal(loadPaneLayers("pane2"), null);
    assert.doesNotThrow(() => savePaneLayers(r));
    delete g.localStorage;
    assert.equal(loadPaneLayers("pane2"), null);
    assert.doesNotThrow(() => savePaneLayers(r));
    const mem = new Map<string, string>();
    g.localStorage = { getItem: (k: string) => mem.get(k) ?? null, setItem: (k: string, v: string) => { mem.set(k, v); } };
    savePaneLayers(r);
    assert.equal(loadPaneLayers("pane2")!.base, "phosphor");
    assert.equal(loadPaneLayers("pane1"), null);
  } finally { if (real) g.localStorage = real; else delete g.localStorage; }
});

test("MAP-06: the surface routes its overlays through the registry, into the one marks hook", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.equal([...src.matchAll(/\bmarks: \(/g)].length, 1, "a second overlay path appeared");
  // The marks hook's body, then what it is about: every registry overlay it draws comes out of
  // `composeOverlays` over THIS pane's registry (`layersFor(pane.id)`, possibly split into z bands
  // around marks drawn outside the registry) and the one renderer table — asserted on the calls,
  // not on the hook's exact text, so a layer ticket that re-orders the bands does not break it.
  const hook = /marks: \(pane, edge\) => \{([^]*?)\n {8}\},\n/.exec(src);
  assert.ok(hook, "the one marks hook");
  const calls = [...hook[1].matchAll(/composeOverlays\((.+?), overlayFns, pane, edge\)/g)];
  assert.ok(calls.length >= 1, "the marks hook is not composed from the registry's renderer table");
  assert.equal(calls.length, [...hook[1].matchAll(/composeOverlays\(/g)].length, "a composeOverlays call not over overlayFns");
  for (const c of calls) {
    assert.match(c[1], /^(layersFor\(pane\.id\)|reg|band\(.*\))$/, `composeOverlays over something other than the pane's registry: ${c[1]}`);
  }
  if (calls.some((c) => !c[1].startsWith("layersFor("))) {
    assert.match(hook[1], /const reg = layersFor\(pane\.id\);/, "the marks hook is not composed from the pane's registry");
  }
  if (calls.some((c) => c[1].startsWith("band("))) {
    assert.match(hook[1], /const band = \(keep: \(z: number\) => boolean\) => \(\{ \.\.\.reg, layers: reg\.layers\.filter\(\(l\) => keep\(l\.z\)\) \}\);/,
      "a z band is not a filter of the pane's own registry");
  }
  // The renderer table, then the entries this check is about: capture rules and detections are drawn
  // by their registry renderers (other layer tickets add their own entries alongside).
  const fns = /overlayFns: Partial<Record<LayerId, OverlayLayerFn>> = \{([^}]*)\}/.exec(src);
  assert.ok(fns, "the overlay renderer table the marks hook composes from");
  assert.match(fns[1], /\brules: ringQuads\b/);
  assert.match(fns[1], /\bdetections: detectionQuads\b/);
  assert.match(fns[1], /\bartifacts: artifactQuads\b/);
  assert.match(fns[1], /\bpaths: pathQuadsFn\b/);
  assert.match(fns[1], /\bpriors: priorsQuads\b/);
  // The base style reaches the trace by the pane's own registry, per frame.
  assert.match(src, /const phosphor = layersFor\(pane\.id\)\.base === "phosphor";/);
  // A split inherits the creating pane's registry.
  assert.match(src, /store\.set\(inheritPane\(from, p\.activePane, seedFor\(from\)\)\)/);
});

test("T-821: research and collection marks paint in z order — between the registry's bands around COLLECTION_Z", () => {
  // Research (z 30) and collections (z COLLECTION_Z = 40) are drawn outside the registry, so the
  // hook must split the registry's overlays around them: below first, the research marks, then the
  // registry overlays ABOVE (artifacts z 50, priors z 60). Appending them after every overlay would
  // paint collections over artifacts.
  assert.equal(COLLECTION_Z, 40);
  assert.ok(layerDef("research")!.z < COLLECTION_Z && layerDef("detections")!.z < COLLECTION_Z);
  assert.ok(layerDef("artifacts")!.z > COLLECTION_Z && layerDef("priors")!.z > COLLECTION_Z);
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  const hook = /marks: \(pane, edge\) => \{([^]*?)\n {8}\},\n/.exec(src);
  assert.ok(hook, "the one marks hook");
  const below = hook[1].indexOf("composeOverlays(band((z) => z < COLLECTION_Z), overlayFns, pane, edge)");
  const research = hook[1].indexOf("markQuads(researchBoxesFor(pane), edge, pane.box, pane.rect)");
  const above = hook[1].indexOf("composeOverlays(band((z) => z > COLLECTION_Z), overlayFns, pane, edge)");
  assert.ok(below >= 0 && research > below && above > research, "paint order is not below-band, research/collections, above-band");
});
