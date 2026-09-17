// Surfaces that describe THE RUN, not THE AIR — and therefore say so (T-387).
//
// CLAUDE.md's whole-UI rule scopes every surface to one (time × frequency) window and obliges it to
// show all the data it has for that window. T-387 asked the prior question for the four surfaces
// T-384 left on the live edge, and three of them answered *live-only*:
//
// | surface | what it describes | why it is not a view of the window |
// |---|---|---|
// | pipelines list | the decoder processes this run has started | a process exists or it does not; it has no past-window form |
// | stage status | each node's lock/quality/error-rate **as it is reading now** | `status` records are telemetry of the decoder, not a measurement of the band, and no route serves them by time (stream contract §14.7 stores them in the capture file but indexes and serves only `frame` records) |
// | outputs dock | the streams **this page** has open, with Mute/Stop/Copy address | a socket this browser tab holds cannot exist in a window an hour ago, and there is nothing there to stop |
//
// Being live-only is not the bug; *looking* windowed while being live-only is. A panel that sits
// beside a scrubbed waterfall and silently answers about now is the same class of lie as the focus
// panel's "no longer in the inventory" (T-385) and the Confirmed list's wall-clock query (T-389):
// the surface makes a claim the user reads as being about the window on screen. So each of the
// three carries this note, in one shared wording, and the note gets *louder* the moment the view
// stops following the live edge — which is exactly when the reader would otherwise be misled.
//
// The fourth surface, the packet inspector, is not here: packets are data about the air, they carry
// capture-clock `t_ns`, and the frames of a past window exist in the pipeline's own capture. It
// re-derives (`decode/inspector.ts`).

/**
 * What a live-only surface says about itself. `subject` names what it *is* a list of, in the
 * caller's own words; the two sentences differ only in the second clause, so a reader who has
 * learned the phrase on one panel reads it the same way on the next.
 *
 * `scrubbed` is "the view is not following the live edge" — the store's `time.live` inverted. It
 * is deliberately the *only* input besides the subject: this note must never depend on whether the
 * surface happens to be empty, or the emptiness would read as the window's answer.
 */
export function liveOnlyNote(subject: string, scrubbed: boolean): string {
  return scrubbed
    ? `live only — ${subject}. The view is scrubbed back; this is not that window.`
    : `live only — ${subject}, not a view of the time window.`;
}

/** The pipelines list's subject (`GET /api/pipelines`: the decoders running in this session). */
export const PIPELINES_SUBJECT = "the decoders running in this session";
/** The stage strip's subject (each node's status readout as it is reading now). */
export const STAGE_STATUS_SUBJECT = "each stage's status as it is reading now";
/** The outputs dock's subject (the streams this page has open). */
export const OUTPUTS_SUBJECT = "the streams this page has open";
