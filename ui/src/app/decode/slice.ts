// Decode workbench state (ADR-0013 §3.1). Owner: T-153. Top-level key: decode. The packet
// inspector's frame/field selection is T-154's, in `decode/inspector-slice.ts`.
import type { AppState } from "../state";

export interface DecodeSlice { pipelineId: string | null; nodeId: string | null }

export interface DecodeState { decode: DecodeSlice }

export const decodeInitial = (): DecodeState => ({ decode: { pipelineId: null, nodeId: null } });

/** Selects a pipeline; its stage selection resets (the new pipeline's nodes differ). */
export const selectPipeline = (pipelineId: string | null) => (): Partial<AppState> => ({ decode: { pipelineId, nodeId: null } });

/** Selects a stage (node) of the current pipeline. */
export const selectNode = (nodeId: string | null) => (s: AppState): Partial<AppState> => ({ decode: { ...s.decode, nodeId } });
