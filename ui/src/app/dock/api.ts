// Cross-task contract (ADR-0013 §8): how other panels add and remove Outputs-dock entries.
// Owner: T-150, which implements these bodies without changing the signatures. Until then they
// are stubs: they toast "not implemented" and add nothing, so T-151/T-153 can code and ship
// against them in parallel.
import type { AppContext } from "../context";
import { toast } from "../state";

/** What to listen to: a known emitter, or a band (a selection or the view). */
export type ListenTarget =
  | { kind: "emitter"; emitterId: string; label: string }
  | { kind: "band"; fLoHz: number; fHiHz: number; label: string };

/** A pipeline output to show as a records entry (`inspector/<pipelineId>/<outputId>`). */
export interface RecordsTarget { pipelineId: string; outputId: string; label: string }

const pending = (ctx: AppContext, what: string) => ctx.store.set(toast(`${what} is not implemented yet (T-150).`));

/**
 * Opens `/ws/open/listen` for `target` and adds an audio entry to the dock. Call it synchronously
 * inside the click handler (mobile audio unlock). The same emitter twice returns the existing
 * entry's id. Returns the dock entry id, or null when nothing was added.
 * Owner: T-150. Stub: toasts and returns null.
 */
export function startListen(ctx: AppContext, target: ListenTarget): string | null {
  void target;
  pending(ctx, "Listen");
  return null;
}

/**
 * Adds a records entry (rate from `GET /api/pipelines` `stats.frames`, Copy address from
 * `/api/streams`) for a pipeline output. Returns the dock entry id, or null when nothing was added.
 * Owner: T-150. Stub: toasts and returns null.
 */
export function startRecordsOutput(ctx: AppContext, target: RecordsTarget): string | null {
  void target;
  pending(ctx, "Stream records");
  return null;
}

/**
 * Stops a dock entry by id: closes its socket (audio) or removes the entry (records; stopping the
 * pipeline itself stays the caller's `DELETE /api/pipelines/{id}`). Unknown ids are ignored.
 * Owner: T-150. Stub: no-op.
 */
export function stopOutput(ctx: AppContext, id: string): void {
  void ctx; void id;
}
