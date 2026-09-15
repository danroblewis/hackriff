// Outputs-dock pure helpers (ADR-0013 §8, §4.8). Owner: T-150. Id generation, listen query/tcp
// target formatting, address text and view-model text — no DOM, no fetch, unit-tested.
import type { ListenTarget } from "./api";
import type { OutputEntry } from "./slice";

let seq = 0;
/** A dock entry id, stable within the page's lifetime (not persisted). */
export function nextId(prefix: string): string {
  seq += 1;
  return `${prefix}${seq}`;
}

/** The `/ws/open/listen` (or TCP `open/listen`) query for a target: `emitter=<id>` or rounded-Hz `f_lo`/`f_hi`. */
export function listenQuery(t: ListenTarget): string {
  const q = new URLSearchParams();
  if (t.kind === "emitter") q.set("emitter", t.emitterId);
  else { q.set("f_lo", String(Math.round(t.fLoHz))); q.set("f_hi", String(Math.round(t.fHiHz))); }
  return q.toString();
}

/** The TCP handshake target for a listen entry (`GET /api/streams` `tcp.handshake` shape). */
export function listenTcpTarget(t: ListenTarget): string {
  return `open/listen?${listenQuery(t)}`;
}

/** The TCP handshake target for a pipeline's stored-records output (stream-contract `inspector/<pipeline>/<output>`). */
export function recordsTcpTarget(pipelineId: string, outputId: string): string {
  return `inspector/${pipelineId}/${outputId}`;
}

/** "Copy address" text: `tcp://<addr> <tcp_target>` — the token is never included (§8 T-150 acceptance). */
export function copyAddressText(tcpAddr: string, tcpTarget: string): string {
  return `tcp://${tcpAddr} ${tcpTarget}`;
}

/** The dock header's count line, matching the mockup's "N live · K pipelines" wording. */
export function outputsCountText(outputs: readonly OutputEntry[]): string {
  const pipelines = outputs.filter((o) => o.kind === "records").length;
  return `${outputs.length} live · ${pipelines} pipeline${pipelines === 1 ? "" : "s"}`;
}

/** An audio entry's sub-line from its stream header, once known (e.g. "WFM audio · 48 kHz"). */
export function audioSubText(mode: string | undefined, sampleRateHz: number | undefined): string {
  const parts: string[] = [];
  if (mode) parts.push(`${mode.toUpperCase()} audio`);
  if (sampleRateHz) parts.push(`${Math.round(sampleRateHz / 1000)} kHz`);
  return parts.length ? parts.join(" · ") : "audio";
}

/** A meter bar's fill percent from a level in dBFS (roughly −80…0 dBFS mapped to 6…100 %; null = idle). */
export function levelPct(dbfs: number | null): number {
  if (dbfs === null || !Number.isFinite(dbfs)) return 6;
  return Math.max(6, Math.min(100, ((dbfs + 80) / 60) * 100));
}

/** A refusal's status line (`refused (403): …`, or just the reason for a client-side check). */
export function refusalText(status: number, reason: string): string {
  return status > 0 ? `refused (${status}): ${reason}` : reason;
}
