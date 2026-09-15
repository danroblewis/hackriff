// Cross-task contract (ADR-0013 §3.2, §8): one shared `/ws/open/inspector?pipeline=<id>` socket per
// pipeline, reference-counted across subscribers, so the workbench (T-153: status → quality tiles)
// and the packet inspector (T-154: frames) never open two sockets to the same pipeline.
// Owner: T-153, which implements the body without changing the signatures. Until then this is a
// stub: subscribing delivers nothing and the returned unsubscribe is a no-op.
import type { AppContext } from "../context";

/** A status record (stream-contract §5), keys as served (`<node>.lock`, `<node>.quality`, …). */
export interface StatusUpdate { tS: number; values: Readonly<Record<string, number | string | boolean | null>> }

/** A frame record exactly as served (stream-contract §14.3); T-154 builds its view model from it. */
export type FrameRecord = Readonly<Record<string, unknown>>;

/** Feed connection state for inline status; `closed` once the pipeline ended. */
export type FeedState = "connecting" | "live" | "reconnecting" | "closed";

export interface FeedHandlers {
  status?(u: StatusUpdate): void;
  frame?(f: FrameRecord): void;
  state?(s: FeedState, message: string): void;
}

/**
 * Subscribes to a pipeline's inspector stream. The first subscriber opens the socket, the last
 * unsubscribe closes it; reconnects with `backoffMs` while the pipeline is running.
 * Returns the unsubscribe function.
 * Owner: T-153. Stub: delivers nothing.
 */
export function subscribePipelineFeed(ctx: AppContext, pipelineId: string, handlers: FeedHandlers): () => void {
  void ctx; void pipelineId; void handlers;
  return () => {};
}
