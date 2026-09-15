// Decode workbench state (ADR-0013 §3.1). Owner: T-153. Top-level key: decode. The packet
// inspector's frame/field selection is T-154's, in `decode/inspector-slice.ts`.

export interface DecodeSlice { pipelineId: string | null; nodeId: string | null }

export interface DecodeState { decode: DecodeSlice }

export const decodeInitial = (): DecodeState => ({ decode: { pipelineId: null, nodeId: null } });
