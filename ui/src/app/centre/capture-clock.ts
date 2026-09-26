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
//
// ## T-1004: what it records when the viewport is FROZEN
//
// "Forward from now" is the whole of the honesty problem in a split view. With one pane frozen on a
// past signal and one live, the button sat in the viewport menu — per-pane chrome, named for the
// active pane since T-1000 — saying "Record IQ" while the active pane showed ten minutes ago. The
// press then recorded the band *now*, which is neither what the pane shows nor what the wording
// implied. A recording cannot be made from the past (the IQ ring holds what it holds; nothing can
// sample a time that has gone), so the answer is not to make the button do something else: it is to
// SAY what the press will record, in the button's own word and in its title, before it is pressed —
// and to say it again in the toast when a frozen viewport's press starts a live recording. Same rule
// as the canvas's grey and its honesty tiers: never imply data the front end cannot deliver.
import type { AppContext } from "../context";
import { h } from "../dom";
import { startPoll } from "../net";
import { setCaptureWindow, setIqAvailability, toast, type AppState } from "../state";
import { timelineRequest } from "../../navigators";
import { captureWindow, currentSpan, iqAvailability, type RecordingsIqResponse, type TimelineResponse } from "./capture-window";

/** How often the capture window is re-read. The rules on the canvas advance per frame from the live
 * edge (`ringRules`), so this only has to catch the ring's *fill* (`buffered.t0_s`) and a
 * reconfigured retention — neither needs the stream's cadence. The raw-IQ-available spans
 * (`state.iqAvailability`) are read on the same cadence: the ring rolls (CLAUDE.md, "Playback": "the
 * boundary is not a constant and must not be cached as one"), so this must be asked afresh, never
 * derived once and held. */
export const CAPTURE_CLOCK_MS = 5_000;

/** The one request the capture clock makes: the window, with no band, no grid to fold. */
export const CAPTURE_CLOCK_REQUEST = timelineRequest(null, 1, 1);

/**
 * `GET /api/recordings`'s `iq_available` (T-469): the ring **and** persisted recordings — the wider
 * of the two audio horizons (T-464), never `/api/coverage` or `/api/tiles`, which answer a different
 * question (observed-vs-unobserved) over a much longer horizon than raw IQ actually survives.
 */
export const IQ_AVAILABILITY_REQUEST = "/api/recordings";

/** Start polling the capture window and the raw-IQ-available spans. Returns the stop function. */
export function startCaptureClock(ctx: AppContext): () => void {
  const { store, client } = ctx;
  return startPoll(async () => {
    const [tl, rec] = await Promise.all([
      client.get<TimelineResponse>(CAPTURE_CLOCK_REQUEST).catch(() => null),
      client.get<RecordingsIqResponse>(IQ_AVAILABILITY_REQUEST).catch(() => null),
    ]);
    // A failed read is *not answered*, which is `null` — unknown, never a window (or horizon) of
    // our own invention.
    store.set(setCaptureWindow(captureWindow(tl)));
    store.set(setIqAvailability(rec === null ? null : iqAvailability(rec)));
  }, CAPTURE_CLOCK_MS);
}

interface RecordSession { id: string; active: boolean; elapsed_s: number; max_s: number }

const spanOf = (s: AppState) => currentSpan({ live: s.live.view, device: s.device });

/**
 * T-1004: where the pane the button acts on sits on the time axis. `frozen` = that pane is behind
 * the growing edge; `pane` is how the chrome names it ("pane 1 of 2"), or null with one pane. A host
 * that does not supply it is a host with no panes to be frozen, and the button reads as it always did.
 */
export interface RecordPane {
  readonly frozen: boolean;
  readonly pane: string | null;
}

/** The button's word. A frozen viewport's press still records the LIVE band, so the word says so
 * rather than leaving "Record IQ" to be read as "record what I am looking at". */
export const recordLabel = (at: RecordPane | null): string => at?.frozen ? "Record IQ (live)" : "Record IQ";

/** The sentence under the pointer: what a press records, and — for a frozen viewport — what it does
 * NOT record, with the reason. Never promises a clip of the past. */
export function recordTitle(at: RecordPane | null, span: { loHz: number; hiHz: number } | null): string {
  const band = span ? `${(span.loHz / 1e6).toFixed(3)}–${(span.hiHz / 1e6).toFixed(3)} MHz` : "the tuned span";
  const what = `Record raw IQ forward from now over ${band} (GAP-1 interim for exporting a clip from the buffer).`;
  if (!at?.frozen) return what;
  const which = at.pane ? `${at.pane} is` : "This viewport is";
  return `${what} ${which} frozen behind the live edge: raw IQ can only be recorded from now on,`
    + " so this records what arrives next, not the past window on screen.";
}

/** The toast a press from a frozen viewport raises — the same statement, at the moment it becomes
 * true, so the recording that starts is never mistaken for a clip of what is on screen. */
export const frozenRecordNote = (at: RecordPane, span: { loHz: number; hiHz: number } | null): string =>
  `Recording raw IQ forward from now over ${span ? `${(span.loHz / 1e6).toFixed(3)}–${(span.hiHz / 1e6).toFixed(3)} MHz` : "the tuned span"}`
  + ` — not the frozen window ${at.pane ?? "this viewport"} is showing: IQ cannot be recorded from the past.`;

/**
 * The "Record IQ" button: starts an IQ recording over the viewport's band, and stops it.
 *
 * `at` (T-1004) reports the pane the surrounding chrome acts on, read at the moment the label is
 * rendered and again at the press — never cached, since a pane freezes and follows as the user
 * scrubs. Returns the button with a `sync()` the host calls on the frame, so the word matches the
 * pane's own state rather than the state it had when the menu was built.
 */
export function recordIqButton(ctx: AppContext, at: () => RecordPane | null = () => null): HTMLButtonElement & { sync(): void } {
  const { store, client } = ctx;
  const btn = h("button", {
    class: "mini sf-record", type: "button",
  }, "Record IQ") as HTMLButtonElement & { sync(): void };
  let session: RecordSession | null = null;
  let stopPoll: (() => void) | null = null;
  const render = () => {
    const where = at();
    const text = session?.active ? `Stop (${session.elapsed_s.toFixed(0)} s)` : recordLabel(where);
    if (btn.textContent !== text) btn.textContent = text;
    const title = session?.active ? "Stop this IQ recording." : recordTitle(where, spanOf(store.get()));
    if (btn.title !== title) btn.title = title;
    // The scope the word came from, so a test reads what the user reads rather than parsing prose.
    const scope = session?.active ? "recording" : where?.frozen ? "frozen" : "live";
    if (btn.dataset.scope !== scope) btn.dataset.scope = scope;
  };
  btn.sync = render;
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
    const where = at();
    client.post<{ recording: RecordSession }>("/api/outputs/record/start", { band: { f_lo: span.loHz, f_hi: span.hiHz }, kinds: ["iq"] })
      .then((r) => {
        session = r.recording; render(); stopPoll = pollSession();
        // T-1004: the press came from a viewport showing the past — say what actually went to disk.
        if (where?.frozen) store.set(toast(frozenRecordNote(where, span)));
      })
      .catch(() => store.set(toast("Could not start an IQ recording.")));
  });
  render();
  return btn;
}
