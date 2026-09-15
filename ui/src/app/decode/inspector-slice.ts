// Packet inspector state (ADR-0013 §3.1). Owner: T-154. Top-level key: inspector.
import type { AppState } from "../state";

export interface InspectorSlice { frameSeq: number | null; fieldNodeId: number | null }

export interface InspectorState { inspector: InspectorSlice }

export const inspectorInitial = (): InspectorState => ({ inspector: { frameSeq: null, fieldNodeId: null } });

/** Selects a frame by its `metadata.frame` sequence number (or clears the selection with `null`),
 * and clears any field selection: a field belongs to one frame. */
export const selectInspectorFrame = (frameSeq: number | null) => (): Partial<AppState> => ({ inspector: { frameSeq, fieldNodeId: null } });

/** Selects (or clears) a layer-tree field, keeping the current frame selection. */
export const selectInspectorField = (fieldNodeId: number | null) => (s: AppState): Partial<AppState> => ({ inspector: { ...s.inspector, fieldNodeId } });
