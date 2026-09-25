// The surface's mount state, stated on the document (T-907).
//
// Two pages mount the unified surface — the app at `/` and the `/surface.html` preview — and each
// has its own note element with its own wording, so a caller that wants to know "has the surface
// finished addressing, or said why it could not" had to know which page it was on. The ui/e2e
// harness's `waitForSurfaceMounted` knew only the preview's `[data-slot="note"]`, found nothing to
// wait for on the app page, and returned before the surface existed.
//
// One attribute on `<html>`, written by both entry points at the same two moments:
//   - `data-surface="mounted"` once `SurfacePreview` has been constructed over an addressed probe;
//   - `data-surface="failed"` on every abort path, with `data-surface-reason` carrying the words
//     the page put on screen.
// Absent means "still addressing". Presentation state only: nothing reads it back in `ui/src`.

export type SurfaceMountState = "mounted" | "failed";

export function markSurface(state: SurfaceMountState, reason = ""): void {
  const d = document.documentElement.dataset;
  d.surface = state;
  if (reason) d.surfaceReason = reason;
  else delete d.surfaceReason;
}
