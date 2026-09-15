// Packet inspector state (ADR-0013 §3.1). Owner: T-154. Top-level key: inspector.

export interface InspectorSlice { frameSeq: number | null; fieldNodeId: number | null }

export interface InspectorState { inspector: InspectorSlice }

export const inspectorInitial = (): InspectorState => ({ inspector: { frameSeq: null, fieldNodeId: null } });
