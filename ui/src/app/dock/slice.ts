// Outputs state (ADR-0013 §3.1, §4.8). Owner: T-150; T-994 retired the dock BAR, not this state.
// Top-level keys: outputs (the streams this page opened), servedOutputs (the backend's open-output
// records the map's active boxes are drawn from) and outputsStripDismissed. Other panels add entries
// only through `dock/api.ts` (startListen, startRecordsOutput, stopOutput).
import type { AppState } from "../state";
import type { ServedPipeline, ServedRecording } from "./activity";

/** One entry of the Outputs dock: a stream this page opened (audio) or a pipeline output. */
export interface OutputEntry {
  id: string; kind: "audio" | "records"; label: string; sub: string;
  state: "opening" | "live" | "refused" | "ended";
  /** TCP handshake target for "Copy address" (`/api/streams` tcp.addr + this). */
  tcpTarget: string | null;
  muted: boolean; levelDbfs: number | null; recordsPerS: number | null;
  emitterId: string | null; pipelineId: string | null;
  /** The pipeline output this entry shows (`output_id`); null for a legacy Listen chain. */
  outputId: string | null; message: string | null;
}

/** T-994: the latest `GET /api/pipelines` / `GET /api/outputs` answers, as the box badges read them. */
export interface ServedOutputs { readonly pipelines: readonly ServedPipeline[]; readonly recordings: readonly ServedRecording[] }

export interface DockState {
  outputs: readonly OutputEntry[];
  servedOutputs: ServedOutputs;
  /** T-994: the Active-outputs strip was closed while showing exactly these entries
   * ([[outputsKey]]); it stays closed until the set changes (a new output opens). `null` = open. */
  outputsStripDismissed: string | null;
}

export const dockInitial = (): DockState => ({ outputs: [], servedOutputs: { pipelines: [], recordings: [] }, outputsStripDismissed: null });

/** The identity of a set of Outputs entries — what closing the strip remembers. */
export const outputsKey = (outputs: readonly OutputEntry[]): string => outputs.map((o) => o.id).sort().join(",");

/** Whether the Active-outputs strip shows: something is open, and it was not closed on this set. */
export const outputsStripShown = (s: Pick<DockState, "outputs" | "outputsStripDismissed">): boolean =>
  s.outputs.length > 0 && s.outputsStripDismissed !== outputsKey(s.outputs);

export const dismissOutputsStrip = (s: AppState): Partial<AppState> => ({ outputsStripDismissed: outputsKey(s.outputs) });

export const setServedOutputs = (served: ServedOutputs) => (): Partial<AppState> => ({ servedOutputs: served });

/** Adds or replaces an Outputs entry by id. */
export const upsertOutput = (e: OutputEntry) => (s: AppState): Partial<AppState> => {
  const i = s.outputs.findIndex((o) => o.id === e.id);
  return { outputs: i < 0 ? [...s.outputs, e] : s.outputs.map((o, j) => (j === i ? e : o)) };
};

export const removeOutput = (id: string) => (s: AppState): Partial<AppState> =>
  (s.outputs.some((o) => o.id === id) ? { outputs: s.outputs.filter((o) => o.id !== id) } : {});
