// T-806 (MAP-06): the per-pane layer registry — docs/24 §13 (normative), §3/§4 (taxonomy, menu).
//
// ## What a "layer" is here, and why the plane is fixed
//
// "Layer" meant three different mechanisms before this file: a cell-rule/ramp state (`data`), a
// stroke in the overlay pass (`overlay`), and a focusable DOM mark (`dom`). A layer declares its
// plane at registration and cannot change it at runtime (docs/24 §13.1). Planes render in a fixed
// order — data, then overlay, then dom — and `z` orders only WITHIN a plane. So no toggle, order or
// preference can put a stroke beneath a measurement's colour: a layer that wanted to wash colour
// over energy would have to change its plane, which it cannot.
//
// ## The two axes (docs/24 §4)
//
//  - **Base style** — exactly one: the amplitude ramp or phosphor. Pure presentation over the base
//    waterfall.
//  - **Overlay content** — any number, each with its own visibility and a defined `z`.
//
// ## One overlay path, not two (docs/24 §13.2)
//
// Every visible `overlay` layer is a pure `(pane, edgeNs) => OverlayQuad[]`, and [[composeOverlays]]
// concatenates them, sorted by `z`, into the ONE `marks` hook `SurfacePreview` already takes. There
// is still exactly one place overlay geometry is produced and one pass that draws it — `overlay.ts`'s
// program, which has no sampler and no ramp — so the byte-identical-with-overlays-off guard keeps its
// force however many layers exist.
//
// Thin client: this file is presentation state only. It commands nothing, fetches nothing and holds
// no signal logic; toggling a layer never reaches a route (the spy-client empty-call-list rule).
import type { OverlayQuad } from "./minimap";
import type { PaneView } from "./surface";

/** Which pass a layer renders in. Fixed at registration; never changes at runtime. */
export type LayerPlane = "data" | "overlay" | "dom";

export type LayerId =
  | "base" | "coverage" | "tier"
  | "detections" | "density" | "paths" | "tune" | "frontend" | "artifacts" | "priors" | "rules" | "research"
  | `collection:${string}`
  | "pins";

export type BaseStyle = "ramp" | "phosphor";

/** A registered layer kind: its fixed plane, its z within that plane, and its default. */
export interface LayerDef {
  readonly id: LayerId;
  readonly plane: LayerPlane;
  readonly z: number;
  readonly visibleByDefault: boolean;
  readonly label: string;
  readonly hint: string;
}

/** Plane paint order. Never re-sorted by anything a user can reach. */
export const PLANE_ORDER: readonly LayerPlane[] = ["data", "overlay", "dom"];

/**
 * Every layer kind docs/24 §13 names, with its plane, `z` and default (§13.4). MAP-07…MAP-13 each
 * wire a renderer for one of these; a kind with no renderer yet is not offered in the menu (a
 * switch that draws nothing would be a control that lies).
 */
export const LAYER_DEFS: readonly LayerDef[] = [
  // `base` is the base-style axis's own layer: always drawn, styled by `PaneLayers.base`.
  { id: "base", plane: "data", z: 0, visibleByDefault: true, label: "Spectrum energy", hint: "the waterfall" },
  { id: "coverage", plane: "data", z: 10, visibleByDefault: true, label: "Coverage fog", hint: "grey · unknown · excluded" },
  { id: "tier", plane: "data", z: 20, visibleByDefault: false, label: "Honesty-tier bands", hint: "per pane" },
  { id: "rules", plane: "overlay", z: 10, visibleByDefault: true, label: "Capture rules", hint: "retention bound · oldest IQ" },
  // T-810 (MAP-10, docs/23 §10.6 rule 6 last bullet): features per cell from `/api/tiles/events`,
  // drawn only where a box there would already generalize to a symbol (`./density.ts`) — the GIS
  // "aggregate at small scale", never a numbered cluster bubble. Sits below `detections` so a box
  // large enough to draw always wins the pixel; a density cell only shows where none would appear.
  { id: "density", plane: "overlay", z: 15, visibleByDefault: true, label: "Density (coarse zoom)", hint: "features per cell · resolves to boxes on zoom-in" },
  { id: "detections", plane: "overlay", z: 20, visibleByDefault: true, label: "Detections", hint: "confirmed · candidate · unknown" },
  // T-897 (docs/23 §10.6 rule 2): traced (t, f) routes — a chirp's diagonal, a sweep's sawtooth, a
  // hopper's staircase — over the boxes they belong to and under the user's own research marks.
  { id: "paths", plane: "overlay", z: 25, visibleByDefault: true, label: "Paths", hint: "chirps · sweeps · hops" },
  // T-898 (docs/23 §10.6 rule 2): the DEVICE's own route through frequency - a directions line per
  // front end, from the recorded tune intervals. Above the measured paths, under the user's marks.
  { id: "tune", plane: "overlay", z: 26, visibleByDefault: true, label: "Retune history", hint: "where each radio has been tuned" },
  // T-981: front-end events — rows where the ADC clipped and the whole window lifted — marked as the
  // radio's own energy, never a signal. On by default: hiding it would let the stripe read as signal.
  { id: "frontend", plane: "overlay", z: 27, visibleByDefault: true, label: "Front-end overload", hint: "clipped rows · the radio's energy, not a signal" },
  { id: "research", plane: "overlay", z: 30, visibleByDefault: false, label: "Research", hint: "measurements · annotations" },
  { id: "artifacts", plane: "overlay", z: 50, visibleByDefault: false, label: "Artifacts", hint: "image · harmonic · IMD" },
  { id: "priors", plane: "overlay", z: 60, visibleByDefault: false, label: "Band-plan priors", hint: "suggestions, never truth" },
  // T-910: the map is GIS — a detection is drawn by `detections` as its polygon (or, under ~6 px,
  // its generalized symbol). This DOM layer is what identifies them: labels, hit areas, keyboard.
  { id: "pins", plane: "dom", z: 10, visibleByDefault: true, label: "Feature labels", hint: "labels · select · keyboard" },
];

/** A durable collection toggled as a layer (MAP-21) sits at z 40, between research and artifacts. */
export const COLLECTION_Z = 40;

export const BASE_STYLES: readonly { id: BaseStyle; label: string; hint: string }[] = [
  { id: "ramp", label: "Amplitude ramp", hint: "trace coloured on the waterfall's own ramp" },
  { id: "phosphor", label: "Phosphor", hint: "one green ink with afterglow, like an analyser's CRT" },
];

export interface Layer {
  readonly id: LayerId;
  readonly plane: LayerPlane;
  readonly z: number;
  readonly visible: boolean;
}

/** A pane's registry: presentation state only. It commands nothing and holds no signal logic. */
export interface PaneLayers {
  readonly paneId: string;
  /** The base-style axis: exactly one. */
  readonly base: BaseStyle;
  /** The overlay-content axis: any number. */
  readonly layers: readonly Layer[];
}

const DEF_BY_ID = new Map<string, LayerDef>(LAYER_DEFS.map((d) => [d.id, d]));

/** The def for an id; a `collection:*` id resolves to a synthesised overlay def. */
export function layerDef(id: LayerId): LayerDef | null {
  const d = DEF_BY_ID.get(id);
  if (d) return d;
  if (id.startsWith("collection:")) {
    return { id, plane: "overlay", z: COLLECTION_Z, visibleByDefault: true, label: id.slice(11), hint: "my collection" };
  }
  return null;
}

/** The default registry for a fresh pane (docs/24 §13.4). Unknown and Candidate detections are
 * visible by default: `detections` defaults on, and nothing here filters it to "explained only". */
export function defaultPaneLayers(paneId: string): PaneLayers {
  return {
    paneId,
    base: "ramp",
    layers: LAYER_DEFS.map((d) => ({ id: d.id, plane: d.plane, z: d.z, visible: d.visibleByDefault })),
  };
}

/** A new pane inherits the creating pane's registry BY VALUE and diverges thereafter (§13.5). */
export function inheritPaneLayers(from: PaneLayers, paneId: string): PaneLayers {
  return { paneId, base: from.base, layers: from.layers.map((l) => ({ ...l })) };
}

export function withBase(reg: PaneLayers, base: BaseStyle): PaneLayers {
  return reg.base === base ? reg : { ...reg, base };
}

/** Set one layer's visibility. The plane and `z` come from the def and are never taken from the
 * caller, so a toggle cannot move a layer across planes. `base` is always drawn — its axis is the
 * style choice, not visibility — so it refuses to hide. */
export function withLayer(reg: PaneLayers, id: LayerId, visible: boolean): PaneLayers {
  if (id === "base") return reg;
  const i = reg.layers.findIndex((l) => l.id === id);
  if (i >= 0) {
    if (reg.layers[i].visible === visible) return reg;
    const layers = reg.layers.slice();
    layers[i] = { ...layers[i], visible };
    return { ...reg, layers };
  }
  const d = layerDef(id);
  if (!d) return reg;
  return { ...reg, layers: [...reg.layers, { id, plane: d.plane, z: d.z, visible }] };
}

export function isLayerVisible(reg: PaneLayers, id: LayerId): boolean {
  const l = reg.layers.find((x) => x.id === id);
  if (l) return l.visible;
  return layerDef(id)?.visibleByDefault ?? false;
}

/** The layers in paint order: by plane (data → overlay → dom), then ascending `z`. */
export function paintOrder(reg: PaneLayers): Layer[] {
  const rank = (p: LayerPlane) => PLANE_ORDER.indexOf(p);
  return reg.layers.slice().sort((a, b) => rank(a.plane) - rank(b.plane) || a.z - b.z);
}

export type OverlayLayerFn = (pane: PaneView, edgeNs: number) => readonly OverlayQuad[];

/**
 * The ONE marks hook's body: every visible `overlay` layer that has a renderer, in ascending `z`,
 * concatenated. A renderer registered for a `data` or `dom` layer is ignored — only the stroke pass
 * is composed here, so a data layer cannot sneak geometry in by being passed as a function.
 */
export function composeOverlays(
  reg: PaneLayers, fns: Partial<Record<LayerId, OverlayLayerFn>>, pane: PaneView, edgeNs: number,
): OverlayQuad[] {
  const out: OverlayQuad[] = [];
  for (const l of paintOrder(reg)) {
    if (l.plane !== "overlay" || !l.visible) continue;
    const fn = fns[l.id];
    if (fn) out.push(...fn(pane, edgeNs));
  }
  return out;
}

// ---- persistence: per viewer, in localStorage; must render correctly when storage is absent ----
//
// Stored as `{ [paneId]: { base, visible } }`: each pane's own registry, so a reload restores each
// pane's layers and an edit on one pane can never become another pane's default. (A split does not
// read storage — the new pane inherits its creator's registry by value.)

export const LAYERS_KEY = "hk-map-layers";

type StoredPane = { base?: unknown; visible?: unknown };

function readStored(raw: string | null): Record<string, StoredPane> {
  if (!raw) return {};
  try {
    const j = JSON.parse(raw) as unknown;
    return j && typeof j === "object" && !Array.isArray(j) ? j as Record<string, StoredPane> : {};
  } catch { return {}; }
}

/** Parse a pane's stored registry defensively: anything unreadable is the defaults, never an
 * error. A stored plane or `z` is ignored — those come from the defs — so storage cannot reorder
 * planes. Returns `null` when nothing is stored for this pane. */
export function parsePaneLayers(raw: string | null, paneId: string): PaneLayers | null {
  const j = readStored(raw)[paneId];
  if (!j || typeof j !== "object") return null;
  let reg = defaultPaneLayers(paneId);
  if (j.base === "ramp" || j.base === "phosphor") reg = withBase(reg, j.base);
  if (j.visible && typeof j.visible === "object") {
    for (const [id, v] of Object.entries(j.visible as Record<string, unknown>)) {
      if (typeof v === "boolean" && layerDef(id as LayerId)) reg = withLayer(reg, id as LayerId, v);
    }
  }
  return reg;
}

/** `raw` with `reg` stored under its pane id; other panes' entries are kept. */
export function serializePaneLayers(raw: string | null, reg: PaneLayers): string {
  const all = readStored(raw);
  const visible: Record<string, boolean> = {};
  for (const l of reg.layers) if (l.id !== "base") visible[l.id] = l.visible;
  all[reg.paneId] = { base: reg.base, visible };
  return JSON.stringify(all);
}

function readRaw(): string | null {
  try { return localStorage.getItem(LAYERS_KEY); } catch { return null; }
}

/** This pane's stored registry, or `null` if none (or storage is unavailable). */
export function loadPaneLayers(paneId: string): PaneLayers | null {
  return parsePaneLayers(readRaw(), paneId);
}

export function savePaneLayers(reg: PaneLayers): void {
  try { localStorage.setItem(LAYERS_KEY, serializePaneLayers(readRaw(), reg)); } catch { /* storage unavailable */ }
}
