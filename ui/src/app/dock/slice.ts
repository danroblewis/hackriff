// Outputs-dock state (ADR-0013 §3.1, §4.8). Owner: T-150. Top-level key: outputs. Other panels
// add entries only through `dock/api.ts` (startListen, startRecordsOutput, stopOutput).
import type { AppState } from "../state";

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

export interface DockState { outputs: readonly OutputEntry[] }

export const dockInitial = (): DockState => ({ outputs: [] });

/** Adds or replaces an Outputs entry by id. */
export const upsertOutput = (e: OutputEntry) => (s: AppState): Partial<AppState> => {
  const i = s.outputs.findIndex((o) => o.id === e.id);
  return { outputs: i < 0 ? [...s.outputs, e] : s.outputs.map((o, j) => (j === i ? e : o)) };
};

export const removeOutput = (id: string) => (s: AppState): Partial<AppState> =>
  (s.outputs.some((o) => o.id === id) ? { outputs: s.outputs.filter((o) => o.id !== id) } : {});
