// **Where an annotation gesture goes** (T-820 / MAP-20, docs/25 §5, docs/api.md "Annotations").
//
// The surface's Annotate/Pin tool modes (`surface/input.ts`) hand the mount either a rectangle
// (a box stroke) or a point (a tap: a text note in Annotate, a marker in Pin), in (Hz, capture-ns).
// This file turns that into the one write the store accepts — `POST /api/annotations` with
// `{kind, f_lo_hz, f_hi_hz, t0_s, t1_s, label, view}` — and builds the windowed `GET` that reads
// them back, so a note drawn now is on the canvas after a reload.
//
// **Provenance is the server's.** The client sends only `view` — what the pane was showing at the
// instant of the gesture (the same stamp a measurement carries, `./measure.ts`); `actor`, `author`,
// `authored_s` and `sample_rate_hz` are added server-side and are never in a body (`400` if they
// were). **No signal logic**: an annotation is a human note, never detection input, and nothing here
// decides anything about what is on the air. It reaches no device route.
import type { AppContext } from "../context";
import type { AnnotationKind, MarkAnnotation } from "../../surface/annotations";
import type { MarkRegion } from "../../surface/marks";
import type { MeasureView } from "./measure";
import { apiErrorText } from "./format";
import { toast } from "../state";

const S_TO_NS = 1e9;
/** The store's label bound (docs/api.md: 1–120 characters, trimmed). */
export const LABEL_MAX = 120;
/** The most a window read asks for — the route's own maximum. */
export const LIST_LIMIT = 2000;

/** The `view` an annotation is stamped with: identical in shape to a measurement's. */
export type AnnotationView = MeasureView;

/** A label as the store will accept it, or `null` (cancelled, empty, or only whitespace). Longer
 * than the bound is truncated rather than refused — the user typed it, and a note cut at 120
 * characters is closer to what they meant than no note. */
export function normLabel(raw: string | null | undefined): string | null {
  if (raw == null) return null;
  const t = raw.trim();
  return t ? t.slice(0, LABEL_MAX) : null;
}

export interface AnnotationRequest {
  kind: AnnotationKind;
  f_lo_hz: number;
  f_hi_hz: number;
  t0_s: number;
  t1_s: number;
  label: string;
  view: AnnotationView;
}

/** The body for a box stroke. `null` when the rectangle is flat on either axis — the store refuses
 * a zero-extent box, so the client never sends one. */
export function boxRequest(r: MarkRegion, label: string, view: AnnotationView): AnnotationRequest | null {
  if (!(r.f1Hz > r.f0Hz) || !(r.t1Ns > r.t0Ns)) return null;
  return { kind: "box", f_lo_hz: r.f0Hz, f_hi_hz: r.f1Hz, t0_s: r.t0Ns / S_TO_NS, t1_s: r.t1Ns / S_TO_NS, label, view };
}

/** The body for a point: a text note or a marker, zero-area at the tapped (Hz, capture instant). */
export function pointRequest(
  kind: "text" | "marker", p: { fHz: number; tNs: number }, label: string, view: AnnotationView,
): AnnotationRequest {
  const t = p.tNs / S_TO_NS;
  return { kind, f_lo_hz: p.fHz, f_hi_hz: p.fHz, t0_s: t, t1_s: t, label, view };
}

/** The windowed read: every annotation whose box intersects `[f0, f1] × [t0, t1]` (Hz, capture s). */
export function annotationsPath(w: { f0Hz: number; f1Hz: number; t0S: number; t1S: number }): string {
  const q = new URLSearchParams({
    f_lo: String(Math.max(0, Math.floor(w.f0Hz))), f_hi: String(Math.ceil(w.f1Hz)),
    t0: String(w.t0S), t1: String(w.t1S), limit: String(LIST_LIMIT),
  });
  return `/api/annotations?${q.toString()}`;
}

/** Post one annotation and report the outcome. Returns what the server stored, or `null` on a
 * failure (reported as a toast; nothing is drawn that was not stored). */
export async function commitAnnotation(ctx: AppContext, body: AnnotationRequest): Promise<MarkAnnotation | null> {
  try {
    const saved = await ctx.client.post<MarkAnnotation>("/api/annotations", body);
    ctx.store.set(toast(`Annotated (${body.kind}): ${body.label}`));
    return saved;
  } catch (e) {
    ctx.store.set(toast(`Annotate: ${apiErrorText(e)}`));
    return null;
  }
}

/** Read the annotations in a window (one page of up to [[LIST_LIMIT]], newest first). */
export async function fetchAnnotations(
  ctx: Pick<AppContext, "client">, w: { f0Hz: number; f1Hz: number; t0S: number; t1S: number },
): Promise<MarkAnnotation[]> {
  const r = await ctx.client.get<{ annotations: MarkAnnotation[] }>(annotationsPath(w));
  return Array.isArray(r?.annotations) ? r.annotations : [];
}
