// The window's stored frames: `GET /api/captures` → `GET /api/captures/{id}/frames?from_t&to_t`.
//
// A pipeline's decoded frames are recorded to a capture with no request (stream contract §14.7), so
// the frames of a *past* window exist even though the live tap — `/ws/open/inspector`, a socket
// with no history form — cannot replay them. The whole-UI window rule makes fetching them
// obligatory rather than optional: the data exists for the window, so it must be shown.
//
// **This is why T-387 needed no contract change.** `docs/api.md` already calls this route "the
// right route for the packet inspector's own scrubbing": it is keyed by capture, carries
// `from_t`/`to_t`, and serves raw stream records at frame granularity — which is exactly what the
// inspector renders. A route that exists beats an opener that would have to grow a history form.
//
// Owned here rather than in `plots.ts` (T-384, which introduced them) so that `inspector.ts` — lazy
// loaded into Explore's chunk by `explore/output-panel.ts` — can reach them without dragging the
// workbench's SVG plotting code along (ADR-0013 §1 gzip budget).
import type { ViewWindow } from "../explore/inventory";

/** A `GET /api/captures` row, narrowed to the fields these callers read. `pipeline_id` is the
 * pipeline→capture key: the packet inspector and the frame tallies are both keyed by pipeline, and
 * this is the only field that follows one to its recorded frames. */
export interface CaptureLite { id: string; pipeline_id: string; t_last: number }

/** Frames per page: the route's documented maximum, so a dense window is **truncated rather than
 * widened** (T-379 obligation 4). */
export const CAPTURE_FRAME_LIMIT = 500;

/**
 * The most recently written capture of `pipelineId`, or `null`.
 *
 * Newest by `t_last` — the capture clock, like everything else that places data in time. A pipeline
 * that rolled its capture has several; the newest is the one whose frames reach the live edge, and
 * a window older than it is a gap this returns nothing for rather than guessing at.
 */
export function captureFor(captures: readonly CaptureLite[], pipelineId: string): CaptureLite | null {
  return captures.filter((c) => c.pipeline_id === pipelineId).sort((a, b) => b.t_last - a.t_last)[0] ?? null;
}

/** Frames of one capture over exactly the view window. `from_t`/`to_t` are Unix **seconds** on the
 * capture clock, the same clock as `w` (docs/api.md "Scrubbing"); a frame record's own `t_ns` is
 * nanoseconds, which is the units law this query has to respect — the route refuses a nanosecond
 * value here rather than misreading it. */
export function captureFramesPath(captureId: string, w: ViewWindow): string {
  return `/api/captures/${encodeURIComponent(captureId)}/frames?from_t=${w.t0}&to_t=${w.t1}&limit=${CAPTURE_FRAME_LIMIT}`;
}
