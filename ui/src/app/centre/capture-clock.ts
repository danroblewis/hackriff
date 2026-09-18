// The capture clock and "Record IQ", re-homed from the retired Capture panel (T-506).
//
// ## The capture clock (T-379)
//
// `state.captureWindow` is the UI's non-stream live edge: `explore/inventory.ts`'s `liveEdgeS`, the
// canvas's own edge (`./surface.ts`) and the History catalogue's default period all fall back to
// it before the first spectrum row arrives, and whenever the stream is down. The Capture panel was
// its **only writer**, so deleting the panel without this poll would have brought back the T-379
// defect by omission: every one of those readers would drop to "unknown", and the next person to
// "fix" that would reach for `Date.now()` — a clock a replay or the mock SDR is not on.
//
// It asks for the window alone: `GET /api/timeline` with no band answers `window` and no grid
// (docs/api.md), so this is the cheapest question the route answers, and the request is built by
// `navigators.ts`'s `timelineRequest` so `ui/test` can assert exactly what goes on the wire.
//
// ## Record IQ (GAP-1 interim for "Export clip from the buffer")
//
// Records forward from now over the viewport's frequency span, not from the retained past. It was a
// button in the panel's header with no other home; it is now one in the canvas bar.
import type { AppContext } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { setCaptureWindow, toast, type AppState } from "../state";
import { timelineRequest } from "../../navigators";
import { captureWindow, currentSpan, type TimelineResponse } from "./capture-window";

/** How often the capture window is re-read. The rules on the canvas advance per frame from the live
 * edge (`ringRules`), so this only has to catch the ring's *fill* (`buffered.t0_s`) and a
 * reconfigured retention — neither needs the stream's cadence. */
export const CAPTURE_CLOCK_MS = 5_000;

/** The one request the capture clock makes: the window, with no band, no grid to fold. */
export const CAPTURE_CLOCK_REQUEST = timelineRequest(null, 1, 1);

/** Start polling the capture window into `state.captureWindow`. Returns the stop function. */
export function startCaptureClock(ctx: AppContext): () => void {
  const { store, client } = ctx;
  return startPoll(async () => {
    const tl = await client.get<TimelineResponse>(CAPTURE_CLOCK_REQUEST).catch(() => null);
    // A failed read is *not answered*, which is `null` — unknown, never a window of our own.
    store.set(setCaptureWindow(captureWindow(tl)));
  }, CAPTURE_CLOCK_MS);
}

interface RecordSession { id: string; active: boolean; elapsed_s: number; max_s: number }

const spanOf = (s: AppState) => currentSpan({ live: s.live.view, device: s.device });

/** The "Record IQ" button: starts an IQ recording over the viewport's band, and stops it. */
export function recordIqButton(ctx: AppContext): HTMLButtonElement {
  const { store, client } = ctx;
  const btn = h("button", {
    class: "mini sf-record", type: "button",
    title: "Record raw IQ forward from now over this viewport's frequency span (GAP-1 interim for exporting a clip from the buffer).",
  }, "Record IQ") as HTMLButtonElement;
  let session: RecordSession | null = null;
  let stopPoll: (() => void) | null = null;
  const render = () => { btn.textContent = session?.active ? `Stop (${session.elapsed_s.toFixed(0)} s)` : "Record IQ"; };
  const pollSession = () => startPoll(async () => {
    if (!session) return;
    const r = await client.get<{ recordings: RecordSession[] }>("/api/outputs");
    const found = r.recordings.find((s) => s.id === session!.id) ?? null;
    session = found;
    render();
    if (!found || !found.active) { store.set(toast("IQ recording finished.")); stopPoll?.(); }
  }, 2000);

  btn.addEventListener("click", () => {
    if (session?.active) {
      client.post<{ recording: RecordSession }>("/api/outputs/record/stop", { id: session.id })
        .then((r) => { session = r.recording; render(); stopPoll?.(); })
        .catch(() => store.set(toast("Could not stop the recording.")));
      return;
    }
    const span = spanOf(store.get());
    if (!span) { store.set(toast("No tuned span to record yet.")); return; }
    client.post<{ recording: RecordSession }>("/api/outputs/record/start", { band: { f_lo: span.loHz, f_hi: span.hiHz }, kinds: ["iq"] })
      .then((r) => { session = r.recording; render(); stopPoll = pollSession(); })
      .catch(() => store.set(toast("Could not start an IQ recording.")));
  });
  render();
  return btn;
}
