// Output recordings (T-061): start per-selection recordings of bits, symbols, WAV audio and IQ
// slices (`POST /api/outputs/record/start`), follow their progress (`GET /api/outputs`), stop them
// (`POST /api/outputs/record/stop`) and offer download links (`GET <url>?token=`). The server
// stores each finished file's Recording/Bitstream row on the selection's links.
import { ControlError, reactionTo } from "./controls/client";
import type { ActionOutcome, Selection } from "./selections";

export type OutputKind = "bits" | "symbols" | "audio" | "iq";
/** Demod: demodulated outputs (Listen also starts when audio is among them). */
export const DEMOD_KINDS: readonly OutputKind[] = ["bits", "symbols", "audio"];
/** Record: the IQ slice plus the demodulated outputs. */
export const RECORD_KINDS: readonly OutputKind[] = ["iq", "bits", "symbols", "audio"];

export interface OutputFile {
  kind: string; file: string; sidecar: string; state: string;
  bytes: number; records: number; dropped_records: number; message: string | null;
  recording_id: string | null; bitstream_id: string | null;
  url: string; sidecar_url: string; extra_urls: string[];
}

export interface OutputSession {
  id: string; active: boolean; selection_id: string | null; emitter_id: string | null;
  f_lo_hz: number; f_hi_hz: number; kinds: string[]; max_s: number; max_bytes: number;
  bytes: number; started_at: string; elapsed_s: number; ended: string | null; links_saved: number;
  files: OutputFile[];
}

export interface OutputsClient {
  get<T>(path: string): Promise<T>;
  post<T>(path: string, body?: unknown): Promise<T>;
}

function failure(verb: string, s: Selection, e: unknown): ActionOutcome {
  const message = e instanceof ControlError ? `${e.code}: ${e.message}` : reactionTo(e).message;
  return { status: "failed", message: `${verb} "${s.name}": ${message}` };
}

/** Starts recording `kinds` for a selection; the outcome links the session to the selection. */
export async function startOutputs(
  client: OutputsClient, s: Selection, kinds: readonly OutputKind[], opts: { max_s?: number; verb?: string } = {},
): Promise<{ outcome: ActionOutcome; session: OutputSession | null }> {
  const verb = opts.verb ?? "record";
  const body: Record<string, unknown> = { selection_id: s.id, kinds: [...kinds] };
  if (opts.max_s !== undefined) body.max_s = opts.max_s;
  try {
    const r = await client.post<{ recording: OutputSession }>("/api/outputs/record/start", body);
    const session = r.recording;
    const refused = session.files.filter((f) => f.state === "refused");
    return {
      session,
      outcome: {
        status: "done",
        message: `${verb} "${s.name}": recording ${session.kinds.join(", ")}${refused.length ? ` (refused: ${refused.map((f) => `${f.kind} ${f.message ?? ""}`.trim()).join("; ")})` : ""}; stop it below`,
        link: { kind: kinds.includes("iq") ? "recording" : "demodulation", target: `output:${session.id}`, note: kinds.join(",") },
      },
    };
  } catch (e) {
    return { outcome: failure(verb, s, e), session: null };
  }
}

/** A download link (plain GETs may carry the token in the query). */
export function downloadHref(url: string, token: string): string {
  return `${url}?token=${encodeURIComponent(token)}`;
}

function fmtBytes(n: number): string {
  if (n >= 1 << 30) return `${(n / (1 << 30)).toFixed(1)} GB`;
  if (n >= 1 << 20) return `${(n / (1 << 20)).toFixed(1)} MB`;
  if (n >= 1 << 10) return `${(n / (1 << 10)).toFixed(1)} kB`;
  return `${n} B`;
}

/** One line of progress for a session. */
export function progressText(s: OutputSession): string {
  const files = s.files.map((f) => `${f.kind} ${f.state}${f.records ? ` ${f.records}` : ""}${f.dropped_records ? ` (${f.dropped_records} dropped)` : ""}`).join(", ");
  const head = s.active
    ? `recording ${s.elapsed_s.toFixed(1)} / ${s.max_s} s · ${fmtBytes(s.bytes)} of ${fmtBytes(s.max_bytes)}`
    : `finished (${s.ended ?? "ended"}) · ${fmtBytes(s.bytes)}${s.links_saved ? ` · ${s.links_saved} link${s.links_saved === 1 ? "" : "s"} saved` : ""}`;
  return `${head} · ${files}`;
}

export interface TrackerOptions {
  onChange: (sessions: readonly OutputSession[]) => void;
  /** Called once when a tracked session stops being active (e.g. reload the selections' links). */
  onFinished?: (s: OutputSession) => void;
  intervalMs?: number;
  schedule?: (fn: () => void, ms: number) => unknown;
}

/** Follows the sessions started in this page: polls while any is active; stop control. */
export class OutputTracker {
  private sessions = new Map<string, OutputSession>();
  private timer: unknown = null;

  constructor(private client: OutputsClient, private opts: TrackerOptions) {}

  list(): OutputSession[] {
    return [...this.sessions.values()].reverse();
  }

  track(s: OutputSession) {
    this.sessions.set(s.id, s);
    this.changed();
    if (s.active) this.later();
    else this.opts.onFinished?.(s);
  }

  private changed() {
    this.opts.onChange(this.list());
  }

  private later() {
    if (this.timer !== null) return;
    const schedule = this.opts.schedule ?? ((fn, ms) => setTimeout(fn, ms));
    this.timer = schedule(() => { this.timer = null; void this.poll(); }, this.opts.intervalMs ?? 1000);
  }

  private update(next: OutputSession) {
    const prev = this.sessions.get(next.id);
    if (!prev) return;
    this.sessions.set(next.id, next);
    if (prev.active && !next.active) this.opts.onFinished?.(next);
  }

  /** Refreshes tracked sessions; schedules another poll while any is active. */
  async poll(): Promise<void> {
    try {
      const r = await this.client.get<{ recordings: OutputSession[] }>("/api/outputs");
      for (const s of r.recordings) this.update(s);
    } catch {
      // Keep the last state; the next poll retries.
    }
    this.changed();
    if (this.list().some((s) => s.active)) this.later();
  }

  /** Stops a session and shows its final state (files finalised). */
  async stop(id: string): Promise<string | null> {
    try {
      const r = await this.client.post<{ recording: OutputSession }>("/api/outputs/record/stop", { id });
      this.update(r.recording);
      this.changed();
      return null;
    } catch (e) {
      return e instanceof ControlError ? `${e.code}: ${e.message}` : reactionTo(e).message;
    }
  }
}
