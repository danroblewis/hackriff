// Centre mounts (ADR-0013 §8). Owner: T-152; rebuilt by T-445's cutover.
//
// The centre used to mount five surfaces — the live spectrum+waterfall, a frequency axis strip and
// the two edge navigators — each drawing or scrubbing the same time–frequency data its own way.
// docs/16 §8.5 retires all of them for one viewport onto the unified surface (`./surface.ts`).
//
// What remains beside it: the tuning nudges (a discrete device action, T-409) and the live-edge
// reader (`./live-edge.ts`), which is the spectrum stream stripped of its renderer — the tuned
// geometry and the capture clock's live edge are state, not a picture.
import type { AppContext, AreaMounts } from "../context";
import { mountLiveEdge } from "./live-edge";
import { mountNudge } from "./nudge";
import { surfaceMounts } from "./surface";

let edgeStarted = false;

/** Mounted on the surface's own slot so the stream starts exactly once, with the centre. */
const withLiveEdge = (el: HTMLElement, ctx: AppContext) => {
  if (!edgeStarted) { edgeStarted = true; mountLiveEdge(ctx); }
  surfaceMounts.surface(el, ctx);
};

export const mounts: AreaMounts = {
  surface: withLiveEdge,
  // T-409: the tuning nudges, beside the frequency controls in the top bar. They move the tuned
  // centre — a device action — so they belong to the centre area and go through `view.ts`'s gate.
  nudge: mountNudge,
};
