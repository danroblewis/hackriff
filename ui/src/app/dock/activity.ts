// T-994: which boxes on the map have an OPEN OUTPUT, and of what kind — the data the map's active
// halo and corner badge draw (`surface/marks.ts` `active`, `surface/pins.ts` badges). Pure: no DOM,
// no fetch, unit-tested (`ui/test/app-outputs-map.test.ts`).
//
// ## Where each kind comes from — the backend's records, never a click
//
// The ticket's rule: "the state comes from the backend's open-output records (listen/stream/decode/
// record sessions), never inferred client-side". So nothing here turns ON because a button was
// pressed; each kind is a record the server served:
//
//  - **audio** — a Listen stream this page holds whose server HEADER has arrived (`state: "live"`,
//    set only by `AudioSession`'s `onHeader`). An `opening` socket the server has not answered yet,
//    a refusal and an ended stream are not active. The level is the server's own status record's
//    `level_dbfs`, never measured from the PCM here.
//  - **decode** — `GET /api/pipelines`: a pipeline with `state: "running"` whose `emitter_id` names
//    the box. An `ended` pipeline drops the badge on the next poll.
//  - **rec** — `GET /api/outputs`: a recording session with `active: true` whose `emitter_id` names
//    the box.
//  - **stream** — a pipeline output this page streams out (the Outputs entry `startRecordsOutput`
//    added), shown only while that pipeline is served `running` against an emitter.
//
// A target that is a band or a selection rather than an emitter draws no box badge (it has no box
// of its own); it still has its chip in the Active outputs strip, where Stop lives.
import { OUTPUT_KINDS, type FeatureActivity, type OutputKind } from "../../surface/badges";
import type { OutputEntry } from "./slice";

export { OUTPUT_KINDS, OUTPUT_KIND_GLYPH, OUTPUT_KIND_WORDS, activityWords, type OutputKind } from "../../surface/badges";

/** `GET /api/pipelines`' pipeline, narrowed to what the badge needs. */
export interface ServedPipeline { readonly id: string; readonly state: string; readonly emitter_id?: string | null }
/** `GET /api/outputs`' recording `Session`, narrowed. */
export interface ServedRecording { readonly id: string; readonly active: boolean; readonly emitter_id?: string | null }

/** A box's open outputs: every kind in [[OUTPUT_KINDS]] order, and the loudest live audio stream's
 * level, 0..1 ([[levelFrac]]) — null with no audio or no level yet. */
export type BoxActivity = FeatureActivity;

/** A server-reported level (dBFS) as 0..1 over roughly −80…−20 dBFS; null when there is none. */
export function levelFrac(dbfs: number | null): number | null {
  if (dbfs === null || !Number.isFinite(dbfs)) return null;
  return Math.max(0, Math.min(1, (dbfs + 80) / 60));
}

/**
 * The open outputs per emitter id. `outputs` are this page's Outputs entries (Listen streams and
 * streamed pipeline outputs); `pipelines`/`recordings` are the latest `GET /api/pipelines` and
 * `GET /api/outputs` answers.
 */
export function boxActivity(
  outputs: readonly OutputEntry[], pipelines: readonly ServedPipeline[], recordings: readonly ServedRecording[],
): Map<string, BoxActivity> {
  const kinds = new Map<string, Set<OutputKind>>();
  const levels = new Map<string, number>();
  const add = (id: string | null | undefined, k: OutputKind) => {
    if (!id) return;
    let s = kinds.get(id);
    if (!s) { s = new Set(); kinds.set(id, s); }
    s.add(k);
  };
  const running = new Map<string, string>();
  for (const p of pipelines) {
    if (p.state !== "running" || !p.emitter_id) continue;
    running.set(p.id, p.emitter_id);
    add(p.emitter_id, "decode");
  }
  for (const r of recordings) if (r.active) add(r.emitter_id, "rec");
  for (const o of outputs) {
    if (o.kind === "audio") {
      if (o.state !== "live" || !o.emitterId) continue;
      add(o.emitterId, "audio");
      const l = levelFrac(o.levelDbfs);
      if (l !== null) levels.set(o.emitterId, Math.max(levels.get(o.emitterId) ?? 0, l));
    } else if (o.pipelineId && running.has(o.pipelineId)) {
      add(running.get(o.pipelineId), "stream");
    }
  }
  const out = new Map<string, BoxActivity>();
  for (const [id, s] of kinds) {
    out.set(id, { kinds: OUTPUT_KINDS.filter((k) => s.has(k)), level: s.has("audio") ? levels.get(id) ?? null : null });
  }
  return out;
}
