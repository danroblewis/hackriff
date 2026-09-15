// Cross-task contract (ADR-0013 §8): how other panels add and remove Outputs-dock entries.
// Owner: T-150, implemented here. Signatures are unchanged from the stub so T-151/T-153 code
// against them without waiting.
import type { AppContext } from "../context";
import { removeOutput, upsertOutput, type OutputEntry } from "../state";
import { AudioSession, type AudioSessionEvents } from "./audio-session";
import { audioSubText, listenTcpTarget, nextId, recordsTcpTarget, refusalText } from "./outputs";

/** What to listen to: a known emitter, or a band (a selection or the view). */
export type ListenTarget =
  | { kind: "emitter"; emitterId: string; label: string }
  | { kind: "band"; fLoHz: number; fHiHz: number; label: string };

/** A pipeline output to show as a records entry (`inspector/<pipelineId>/<outputId>`). */
export interface RecordsTarget { pipelineId: string; outputId: string; label: string }

// One shared AudioSession per page load, created on first use (a fresh worktree/session per
// `main()` call, so a module-level singleton is safe: `dock/index.ts`'s mount always runs before
// any click handler can call startListen, since main.ts mounts every area synchronously at load).
let session: AudioSession | null = null;

function events(ctx: AppContext): AudioSessionEvents {
  const patch = (id: string, fn: (e: OutputEntry) => OutputEntry) => {
    ctx.store.set((s) => {
      const e = s.outputs.find((o) => o.id === id);
      return e ? upsertOutput(fn(e))(s) : {};
    });
  };
  return {
    onHeader(id, header) {
      patch(id, (e) => ({ ...e, state: "live", sub: audioSubText(header.audio?.mode, header.sample_rate_hz), message: null }));
    },
    onRefused(id, status, reason) {
      patch(id, (e) => ({ ...e, state: "refused", message: refusalText(status, reason) }));
    },
    onStatus(id, status) {
      patch(id, (e) => ({ ...e, levelDbfs: status.level_dbfs }));
    },
    onClosed(id, _hadHeader, reason) {
      patch(id, (e) => ({ ...e, state: e.state === "refused" ? "refused" : "ended", message: reason }));
    },
  };
}

/** The shared `AudioSession` (T-150 internal use: `dock/index.ts`'s Mute control). */
export function getAudioSession(ctx: AppContext): AudioSession {
  session ??= new AudioSession(ctx.token, events(ctx));
  return session;
}

/**
 * Opens `/ws/open/listen` for `target` and adds an audio entry to the dock. Call it synchronously
 * inside the click handler (mobile audio unlock). The same emitter twice returns the existing
 * entry's id. Returns the dock entry id, or null when nothing was added.
 * Owner: T-150.
 */
export function startListen(ctx: AppContext, target: ListenTarget): string | null {
  if (target.kind === "emitter") {
    const existing = ctx.store.get().outputs.find((o) => o.kind === "audio" && o.emitterId === target.emitterId);
    if (existing) return existing.id;
  }
  const id = nextId("listen");
  const entry: OutputEntry = {
    id, kind: "audio", label: target.label, sub: "estimating…", state: "opening",
    tcpTarget: listenTcpTarget(target), muted: false, levelDbfs: null, recordsPerS: null,
    emitterId: target.kind === "emitter" ? target.emitterId : null, pipelineId: null, message: null,
  };
  ctx.store.set(upsertOutput(entry));
  getAudioSession(ctx).start(id, target);
  return id;
}

/**
 * Adds a records entry (rate from `GET /api/pipelines` `stats.frames`, Copy address from
 * `/api/streams`) for a pipeline output. Returns the dock entry id, or null when nothing was added.
 * Owner: T-150. The same pipeline output twice returns the existing entry's id.
 */
export function startRecordsOutput(ctx: AppContext, target: RecordsTarget): string | null {
  const existing = ctx.store.get().outputs.find((o) => o.kind === "records" && o.pipelineId === target.pipelineId);
  if (existing) return existing.id;
  const id = nextId("rec");
  const entry: OutputEntry = {
    id, kind: "records", label: target.label, sub: "records → tcp", state: "live",
    tcpTarget: recordsTcpTarget(target.pipelineId, target.outputId), muted: false, levelDbfs: null,
    recordsPerS: null, emitterId: null, pipelineId: target.pipelineId, message: null,
  };
  ctx.store.set(upsertOutput(entry));
  return id;
}

/**
 * Stops a dock entry by id: closes its socket (audio) or removes the entry (records; stopping the
 * pipeline itself stays the caller's `DELETE /api/pipelines/{id}`). Unknown ids are ignored.
 * Owner: T-150.
 */
export function stopOutput(ctx: AppContext, id: string): void {
  const e = ctx.store.get().outputs.find((o) => o.id === id);
  if (!e) return;
  if (e.kind === "audio") getAudioSession(ctx).stop(id);
  ctx.store.set(removeOutput(id));
}
