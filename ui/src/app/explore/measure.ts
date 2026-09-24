// **Where a measurement stroke goes** (T-822 / MAP-22, docs/25 §4 and §10).
//
// The surface's measure-mode drag (`surface/input.ts`'s `onMeasure`) hands the mount a rectangle in
// (Hz, capture-ns) — the same shape `explore/region.ts` gets from a shift-drag. A region has one
// destination decided by what is armed; a measurement's two cursors serve two of docs/25 §4's kinds
// **directly, with no extra intent needed**: `delta_f` (the cursors are apart in frequency) and
// `delta_t` (apart in time). Both are saved when both apply — a diagonal drag is one gesture stating
// two independent facts about the same two cursors, and dropping one silently would be the kind of
// state-that-does-something-the-user-did-not-ask-for `region.ts`'s own header warns about.
//
// `bandwidth`, `duration` and `symbol_rate`/`period` are the **same** two cursors under a stricter
// or richer claim (docs/25 §4/§10.4): `bandwidth`/`duration` need a positive span the cursors do not
// promise just by existing, and a rate/period needs a cycle count `n` no drag alone carries. A plain
// crosshair drag is not evidence of any of those three, so this ticket persists the two that are
// always true of two placed cursors and no more; a kind picker that lets a user assert the others
// explicitly is a natural follow-up (this ticket's handback names it).
//
// **No signal logic, and the value is never computed here.** `POST /api/measurements` carries only
// the cursors and the view context; the server computes value, unit and place
// (`crates/hk-model/src/repo/measurements.rs`). This file decides *which kinds* a stroke supports
// and builds the request — arithmetic over already-known view state, same as `region.ts`.
import type { AppContext } from "../context";
import type { MarkMeasurement, MarkRegion } from "../../surface/marks";
import { apiErrorText } from "./format";
import { toast } from "../state";

const S_TO_NS = 1e9;

/** The `view` a measurement is stamped with: what the pane was showing at the instant of the drag
 * (docs/25 §10.2). The server is the only writer of everything else in `provenance`. */
export interface MeasureView {
  center_hz: number;
  span_hz: number;
  t_capture: [number, number];
  tier: "live-iq" | "spectrum-history" | "survey-overview";
  device_id?: string | null;
}

const KINDS = ["delta_f", "delta_t"] as const;
type Kind = (typeof KINDS)[number];

/** Is this rectangle something a measurement can be taken over at all? Extent on **either** axis is
 * enough — unlike `region.ts`'s `regionIsReal`, which needs both, since a pure vertical (Δt) or
 * pure horizontal (Δf) stroke is still a real measurement, inspectrum-style. */
export const measureIsReal = (r: MarkRegion): boolean => r.f1Hz > r.f0Hz || r.t1Ns > r.t0Ns;

/** Which of docs/25 §4's kinds this stroke's two cursors support, in the order they are saved. A
 * stroke with no extent on an axis supports nothing on that axis: `compute_measurement` itself
 * accepts `delta_f`/`delta_t` at zero, but a zero-width "measurement" the user did not drag for is
 * not a claim worth persisting twice over. */
export function measureKinds(r: MarkRegion): readonly Kind[] {
  const out: Kind[] = [];
  if (r.f1Hz > r.f0Hz) out.push("delta_f");
  if (r.t1Ns > r.t0Ns) out.push("delta_t");
  return out;
}

function cursorsFor(r: MarkRegion): [{ f_hz: number; t_s: number }, { f_hz: number; t_s: number }] {
  return [
    { f_hz: r.f0Hz, t_s: r.t0Ns / S_TO_NS },
    { f_hz: r.f1Hz, t_s: r.t1Ns / S_TO_NS },
  ];
}

/**
 * Commit `region` as one saved measurement per kind [[measureKinds]] finds in it, report the
 * outcome, and return what was saved (empty on a no-op or a failure). Every request shares the same
 * two cursors and `view`; only `kind` differs, since `delta_f` and `delta_t` are two facts about one
 * drag, not two drags — and both records share one place, so a caller drawing a mark needs only one
 * of the returned records' geometry, not one box per kind.
 */
export async function commitMeasurement(
  ctx: AppContext, region: MarkRegion, view: MeasureView, fmt: (r: MarkRegion) => string,
): Promise<readonly MarkMeasurement[]> {
  if (!measureIsReal(region)) return [];
  const kinds = measureKinds(region);
  if (kinds.length === 0) return [];
  const { store } = ctx;
  const saved: MarkMeasurement[] = [];
  try {
    for (const kind of kinds) {
      saved.push(await ctx.client.post<MarkMeasurement>("/api/measurements", { kind, cursors: cursorsFor(region), view }));
    }
    store.set(toast(`Measured: ${fmt(region)}`));
  } catch (e) {
    store.set(toast(`Measure: ${apiErrorText(e)}`));
  }
  return saved;
}
