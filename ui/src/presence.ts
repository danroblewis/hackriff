// Live presence **endpoints** (T-410, ADR-0019): the `presence` stream's records, and the one rule
// for applying one to an inventory row that is already on screen.
//
// Why this exists, and what changed. T-388 built this under **contract A — presence is an
// accumulation of observations**: the box's top was the newest *measured* end, and a record per
// open emitter per tick pushed it forward. It could never over-claim, and it could never say the
// one thing a live spectrum display exists to say — *this signal is on the air now*.
//
// The user replaced it with **contract B — presence is an interval with endpoints**. The box runs
// from its start straight to the live edge and caps only on a real detected END, so the measurement
// is the START event plus the **absence of an END**. The stream therefore carries START / END /
// REOPEN and nothing at all while an interval merely continues — never a per-poll presence bump.
//
// T-413 added REVOKE, which makes the END *provisional*: a signal that resumes within one idle gap
// of a detected end nulls it and the ONE interval continues, on the same row, rather than splitting
// into two boxes. The interval then carries `revoked_s`, the silence it was rejoined across, which
// is what stops "one interval" being read as "on air throughout".
//
// What keeps that honest, here and next door. A box drawn to the live edge is a claim about air
// nobody measured, so:
//
//   * the span above the last measured end is drawn as the **open cap** — lighter, with a rule
//     where measurement stops (`timebox.ts`, `waterfall.ts`) — and it grows visibly as the silence
//     grows, so a suspected end is legible without being acted on;
//   * nothing here reads a clock and nothing here interpolates. `t_end_s` is copied from the
//     record, never advanced: it is the *boundary* of the cap, and the cap's own top is placed by
//     the render pass at the newest row it is drawing (T-362);
//   * an END carries the **measured** end, so capping is the box retracting to the truth rather
//     than stopping wherever the assumption had reached.
//
// Presentation-free and DOM-free, so every rule is unit-tested directly.

import type { PresenceInterval, Row } from "./inventory";

/** `metadata.kind` of an opening record for an emitter's first interval. */
export const PRESENCE_START_KIND = "presence-start";
/** …for a later interval on an emitter that has been on the air before. */
export const PRESENCE_REOPEN_KIND = "presence-reopen";
/** …of a closing record, at the interval's measured end. **Provisional**: revocable for one idle
 * gap (T-413). */
export const PRESENCE_END_KIND = "presence-end";
/** …**withdrawing** an END published within the last idle gap: the signal came back inside the
 * revocation window, so the interval it capped is the SAME interval and is open again (T-413,
 * ADR-0019 §6.1). Never a new interval — that is [[PRESENCE_REOPEN_KIND]], which draws a second
 * box. */
export const PRESENCE_REVOKE_KIND = "presence-revoke";
/** The stream the endpoints arrive on (`/ws/presence`, `GET /api/streams`). */
export const PRESENCE_STREAM_ID = "presence";

/** Which endpoint a record announces. */
export type PresenceEventKind =
  | typeof PRESENCE_START_KIND | typeof PRESENCE_REOPEN_KIND | typeof PRESENCE_END_KIND | typeof PRESENCE_REVOKE_KIND;

const KINDS: readonly string[] = [PRESENCE_START_KIND, PRESENCE_REOPEN_KIND, PRESENCE_END_KIND, PRESENCE_REVOKE_KIND];

/**
 * One endpoint: which emitter, which endpoint, and the `presence.last_interval` object the backend
 * serves for it. The interval is the same three fields `/api/inventory` returns, so it is assigned,
 * not rebuilt.
 */
export interface PresenceEvent {
  kind: PresenceEventKind;
  emitterId: string;
  interval: PresenceInterval;
}

const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/**
 * One NDJSON record from the presence stream, or `null` for anything else it carries — a drop
 * marker, a record of another kind, a malformed line. `null` means "nothing to apply", never "apply
 * something approximate".
 */
export function parsePresenceEvent(text: string): PresenceEvent | null {
  let j: Record<string, unknown>;
  try { j = JSON.parse(text) as Record<string, unknown>; } catch { return null; }
  if (j.type !== "message") return null;
  const meta = j.metadata as Record<string, unknown> | undefined;
  const kind = meta?.kind;
  if (typeof kind !== "string" || !KINDS.includes(kind)) return null;
  const emitterId = typeof j.emitter_id === "string" ? j.emitter_id : null;
  const iv = meta?.last_interval as Record<string, unknown> | undefined;
  if (!emitterId || !iv) return null;
  const t0 = num(iv.t_start_s), t1 = num(iv.t_end_s);
  if (t0 === null || t1 === null || typeof iv.open !== "boolean") return null;
  // `open` is stated by the record and must agree with the kind it arrived as; a record that
  // disagrees with itself is a malformed record, not a judgement call to make here.
  if (iv.open !== (kind !== PRESENCE_END_KIND)) return null;
  const revoked = num(iv.revoked_s);
  return {
    kind: kind as PresenceEventKind, emitterId,
    interval: { t_start_s: t0, t_end_s: t1, open: iv.open, ...(revoked === null ? {} : { revoked_s: revoked }) },
  };
}

/**
 * The row's `presence` with this endpoint applied, or `null` when it must not be applied — in which
 * case the row is left exactly as the last poll served it.
 *
 * **The refusals, under contract B.** T-388 had three; ADR-0019 §7 keeps two and replaces the third.
 *
 * 1. **No interval on the row.** *(Kept verbatim.)* `last_interval: null` means no presence
 *    interval intersects the window being viewed; there is no box, and one conjured from a record
 *    would be a rectangle the windowed query did not return. Rows are created by the poll, never by
 *    the stream.
 * 2. **Nothing may shorten the measured extent.** *(Kept, restated.)* An END at or before an end
 *    already held is dropped, so a reordered or replayed record cannot pull a box's measured edge
 *    backwards, and an opening record may not move a live interval's start later. Capping the open
 *    cap is **not** shortening: that span was assumption standing in for a measurement, and the END
 *    is the measurement it was standing in for.
 * 3. ~~**Not contiguous.**~~ *(Removed.)* An extension whose span started after the end on screen
 *    used to be refused, because stretching the box across the silence would assert the emitter
 *    transmitted through it. A REOPEN does not stretch anything: it **replaces** `last_interval`
 *    with the new one, so the returning signal gets its own box and the silence between them is
 *    drawn as a gap rather than hidden behind a box that quietly stopped moving. Same honesty, and
 *    the box arrives on the next tick instead of on the next poll. The earlier interval is not
 *    lost — it is History's, and the windowed poll still serves it.
 *
 * A START/REOPEN's `t_start_s` is taken from the record, since a new interval is a new start; a
 * continuing interval's is deliberately left as the row had it, because the row's interval may have
 * begun before the track this record came from.
 *
 * **A REVOKE (T-413) is neither an opening nor a closing record and takes neither path.** It says
 * the END just applied is withdrawn: the *same* interval is open again, so the row's own
 * `t_start_s` is kept — a REVOKE that claims a **later** start is addressed to an interval this row
 * is not holding and is refused under (1)'s reasoning, and one that would pull the measured end
 * backwards is refused under (2). Re-opening is not a claim about air: the box goes back to running
 * to the live edge with the open cap above its measured end, exactly as before the END. What *is*
 * new is `revoked_s` — measured silence inside the interval — which the box is drawn with so a
 * rejoined span never reads as continuous transmission. It only ever grows, because the stream
 * reports the silence it could show and the poll reports the whole of it.
 */
export function applyPresenceEvent(row: Pick<Row, "presence">, ev: PresenceEvent): Row["presence"] | null {
  const p = row.presence;
  const iv = p?.last_interval;
  if (!p || !iv) return null; // (1)
  if (ev.kind === PRESENCE_END_KIND) {
    // (2): an END may only ever state an end at or past the measured one already held.
    if (!(ev.interval.t_end_s >= iv.t_end_s)) return null;
    if (!iv.open && ev.interval.t_end_s <= iv.t_end_s) return null; // already closed here
    return { ...p, last_interval: { ...iv, t_end_s: ev.interval.t_end_s, open: false } };
  }
  if (ev.kind === PRESENCE_REVOKE_KIND) {
    if (ev.interval.t_start_s > iv.t_start_s) return null; // a later interval, not the one held
    if (ev.interval.t_end_s < iv.t_end_s) return null; // (2)
    const revoked = Math.max(iv.revoked_s ?? 0, ev.interval.revoked_s ?? 0);
    return { ...p, last_interval: { ...iv, t_end_s: ev.interval.t_end_s, open: true, revoked_s: revoked } };
  }
  // (3): an opening record whose interval is the one already on screen is not news — contract B
  // says nothing while an interval continues, so this is a replay.
  if (ev.interval.t_start_s <= iv.t_start_s) return null;
  return { ...p, last_interval: { ...ev.interval } };
}
