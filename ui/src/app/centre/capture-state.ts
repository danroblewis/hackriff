// **Capture state, stated on the surface itself** (T-508).
//
// The user reported it three times: *"the Retune button kills the live view; `hk serve` stays up;
// it never recovers."* Two defects made that, and one of them was this page. When the backend's run
// ended — for good, on a failed re-plumb — `/api/control/state` said `finished: true` and the only
// place that reached was a nine-character label in the top bar. The canvas went on drawing its last
// rows with a following pane, a "Live" button lit and a growing-edge readout: a frozen edge that
// looked live. Capture could also be *restarting* after a device failure, which read exactly like
// running.
//
// So the canvas says it, over the picture, in a sentence: capture is being restarted, or capture has
// stopped — with the backend's own cause. It is derived only from what the backend reports
// (`run.capture`, `run.capture_note`, `run.finished`), never from a client-side guess about whether
// rows "seem" to be arriving: the thin-client rule, and the only way the statement can be true.
import type { DeviceSlice } from "../shell-slice";

/** What the surface states about capture, or `null` while it runs (nothing to say). */
export interface CaptureBanner {
  readonly state: "recovering" | "ended";
  readonly text: string;
}

/** The banner for a device slice. Pure; tested in `ui/test/app-centre.test.ts`. */
export function captureBanner(d: Pick<DeviceSlice, "loaded" | "live" | "capture" | "captureNote">): CaptureBanner | null {
  if (!d.loaded || d.capture === null || d.capture === "running") return null;
  const cause = d.captureNote ? ` Cause: ${d.captureNote}` : "";
  if (d.capture === "recovering") {
    return {
      state: "recovering",
      text: "Capture interrupted — the front end is being restarted. The live edge is not advancing " +
        `until samples arrive again.${cause}`,
    };
  }
  return {
    state: "ended",
    text: (d.live
      ? "Capture stopped — this run has ended and nothing more will arrive. "
      : "The recording has ended — nothing more will arrive. ") +
      `What is on screen is recorded history, not a live edge.${cause}`,
  };
}
