// Live presence extensions (T-388): the `presence` stream's records, and the one rule for applying
// one to an inventory row that is already on screen.
//
// Why this exists. A live signal's box grew in steps of ten seconds: the backend committed an open
// track's presence every 5 s and the UI polled `/api/inventory` every 5 s, two lazy links in
// series. The backend now publishes how far each open emitter's presence **has been observed** on
// its own stream (docs/stream-contract.md §15), and this file is everything the client does with it.
//
// What it must never do, which is the whole point of pushing rather than guessing. A box may only
// extend as far as presence was actually measured. Nothing here reads a clock, and nothing here
// interpolates between records: `t_end_s` is copied from the record, never advanced towards the live
// edge, so a signal that stops has a box that stops. Drawing to the live edge on the assumption the
// signal is still there would be a claim about air nobody measured — the same rule as the coverage
// map's grey, which refuses to spell "never looked" as "looked and it was quiet".
//
// Presentation-free and DOM-free, so both rules are unit-tested directly.

import type { PresenceInterval, Row } from "./inventory";

/** `metadata.kind` of a presence extension, and the `frame_model` beside it (hk-pipeline). */
export const PRESENCE_EXTENSION_KIND = "presence-extension";
/** The stream the extensions arrive on (`/ws/presence`, `GET /api/streams`). */
export const PRESENCE_STREAM_ID = "presence";

/** One extension: which emitter, and the `presence.last_interval` object the backend now serves for
 * it. The interval is the same three fields `/api/inventory` returns, so it is assigned, not
 * rebuilt. */
export interface PresenceExtension {
  emitterId: string;
  interval: PresenceInterval;
}

const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/**
 * One NDJSON record from the presence stream, or `null` for anything else it carries — a drop
 * marker, a record of another kind, a malformed line. `null` means "nothing to apply", never "apply
 * something approximate".
 */
export function parsePresenceExtension(text: string): PresenceExtension | null {
  let j: Record<string, unknown>;
  try { j = JSON.parse(text) as Record<string, unknown>; } catch { return null; }
  if (j.type !== "message") return null;
  const meta = j.metadata as Record<string, unknown> | undefined;
  if (!meta || meta.kind !== PRESENCE_EXTENSION_KIND) return null;
  const emitterId = typeof j.emitter_id === "string" ? j.emitter_id : null;
  const iv = meta.last_interval as Record<string, unknown> | undefined;
  if (!emitterId || !iv) return null;
  const t0 = num(iv.t_start_s), t1 = num(iv.t_end_s);
  if (t0 === null || t1 === null || typeof iv.open !== "boolean") return null;
  return { emitterId, interval: { t_start_s: t0, t_end_s: t1, open: iv.open } };
}

/**
 * The row's `presence` with this extension applied, or `null` when it must not be applied — in
 * which case the row is left exactly as the last poll served it.
 *
 * Three refusals, each of them a thing the box would otherwise claim without evidence:
 *
 * 1. **No interval on the row.** `last_interval: null` means no presence interval intersects the
 *    window being viewed; there is no box, and one conjured from an extension would be a rectangle
 *    the windowed query did not return.
 * 2. **Not newer.** An extension at or behind the end already held is dropped rather than applied,
 *    so a reordered or replayed record can never shorten a box.
 * 3. **Not contiguous.** An extension whose span starts *after* the end on screen describes a
 *    different stretch of air, with silence in between (a new track bound to the same emitter after
 *    a gap). Stretching the box across that gap would assert the emitter was transmitting through
 *    it. The box waits for the poll, which serves the new interval as its own.
 *
 * `t_start_s` is deliberately left as the row had it: the row's interval may have begun before the
 * track this extension came from, and an extension is only ever news about the newest edge.
 */
export function extendPresence(row: Pick<Row, "presence">, ext: PresenceExtension): Row["presence"] | null {
  const p = row.presence;
  const iv = p?.last_interval;
  if (!p || !iv) return null;
  if (!(ext.interval.t_end_s > iv.t_end_s)) return null;
  if (ext.interval.t_start_s > iv.t_end_s) return null;
  return { ...p, last_interval: { ...iv, t_end_s: ext.interval.t_end_s, open: ext.interval.open } };
}
