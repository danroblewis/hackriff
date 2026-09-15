// Focus-panel model (ADR-0013 §4.5, T-151): pure view-model helpers for the focused signal or
// selection, plus the actions that read fields already served by the API — nothing here estimates
// a signal parameter itself.
import { apiErrorText } from "./format";
import { RECORD_KINDS } from "../../outputs";
import type { Row } from "./inventory";
import type { Selection } from "./selections";

export interface ApiClient { get<T>(path: string): Promise<T>; post<T>(path: string, body?: unknown): Promise<T> }

/** The Decode action's label: names the known identity scheme when there is one (RDS, …), else the
 * generic invitation — never claims a decode will run automatically (GAP 7b: recipes are listed,
 * never auto-started). */
export function decodeActionLabel(r: Pick<Row, "identity_scheme">): string {
  const scheme = r.identity_scheme?.split("-")[0];
  return scheme ? `Decode ${scheme.toUpperCase()}` : "Open in Decode";
}

/** Starts a recording of this emitter forward from now (§4.5 "Record / Export clip"; GAP 1: not
 * from the always-on buffer yet). */
export async function recordEmitterClip(client: ApiClient, emitterId: string): Promise<{ ok: true; kinds: string[] } | { ok: false; message: string }> {
  try {
    const r = await client.post<{ recording: { kinds: string[] } }>("/api/outputs/record/start", {
      emitter_id: emitterId, kinds: RECORD_KINDS,
    });
    return { ok: true, kinds: r.recording.kinds };
  } catch (e) {
    return { ok: false, message: apiErrorText(e) };
  }
}

interface StreamsInfo { tcp: { addr: string } | null; on_demand: { name: string; tcp_target: string }[] }

/** "Stream out" address for an emitter's audio (§4.5; IQ stream-out is GAP 8, not offered here):
 * `tcp://<addr> <tcp_target>?emitter=<id>`, the same shape the dock's Copy address uses, without
 * the token. `null` when no TCP stream server or `listen` opener is offered. */
export async function emitterStreamAddress(client: ApiClient, emitterId: string): Promise<string | null> {
  const r = await client.get<StreamsInfo>("/api/streams");
  const listen = r.on_demand.find((o) => o.name === "listen");
  if (!r.tcp || !listen) return null;
  return `tcp://${r.tcp.addr} ${listen.tcp_target}?emitter=${encodeURIComponent(emitterId)}`;
}

/** Selection panel header text: extent width plus how many emitters are inside. */
export function selectionSummary(s: Selection, insideCount: number): string {
  const wideHz = s.f_hi - s.f_lo;
  const wide = wideHz >= 1e6 ? `${(wideHz / 1e6).toFixed(2)} MHz` : `${Math.round(wideHz / 1e3)} kHz`;
  return `${wide} wide · ${insideCount} signal${insideCount === 1 ? "" : "s"} inside`;
}
