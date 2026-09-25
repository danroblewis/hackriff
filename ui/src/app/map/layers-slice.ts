// Per-pane layer registries (T-806 / MAP-06; docs/24 §13, §15). Top-level key: `layers`.
//
// Ephemeral presentation state only (docs/24 §15 rule 4): which base style and which overlays each
// pane draws. It holds no signal data and nothing a route needs; the surface's frame callback reads
// it fresh every frame (never on a poll — T-388), and the layers menu writes it for the ACTIVE pane.
import type { AppState } from "../state";
import { inheritPaneLayers, withBase, withLayer, type BaseStyle, type LayerId, type PaneLayers } from "../../surface/layers";

export interface LayersState { layers: Readonly<Record<string, PaneLayers>> }

export function layersInitial(): LayersState {
  return { layers: {} };
}

/** A pane's registry, or `fallback` (the viewer's stored template) re-keyed to it. */
export function paneLayersOf(s: LayersState, paneId: string, fallback: PaneLayers): PaneLayers {
  return s.layers[paneId] ?? inheritPaneLayers(fallback, paneId);
}

export const putPaneLayers = (reg: PaneLayers) => (s: AppState): Partial<AppState> =>
  (s.layers[reg.paneId] === reg ? {} : { layers: { ...s.layers, [reg.paneId]: reg } });

export const setPaneBase = (paneId: string, base: BaseStyle, fallback: PaneLayers) => (s: AppState): Partial<AppState> =>
  putPaneLayers(withBase(paneLayersOf(s, paneId, fallback), base))(s);

export const setPaneLayer = (paneId: string, id: LayerId, visible: boolean, fallback: PaneLayers) => (s: AppState): Partial<AppState> =>
  putPaneLayers(withLayer(paneLayersOf(s, paneId, fallback), id, visible))(s);

/** A split: the new pane inherits the creating pane's registry by value (docs/24 §13.5). */
export const inheritPane = (fromId: string, toId: string, fallback: PaneLayers) => (s: AppState): Partial<AppState> =>
  putPaneLayers(inheritPaneLayers(paneLayersOf(s, fromId, fallback), toId))(s);

export const dropPaneLayers = (paneId: string) => (s: AppState): Partial<AppState> => {
  if (!(paneId in s.layers)) return {};
  const layers = { ...s.layers };
  delete layers[paneId];
  return { layers };
};
