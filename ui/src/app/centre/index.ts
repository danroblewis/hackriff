// Centre live view mounts (ADR-0013 §8). Owner: T-152.
import type { AreaMounts } from "../context";
import { mountAxis } from "./axis-view";
import { mountLiveSpectrum } from "./live-spectrum";
import { navigatorMounts } from "./navigators";
import { mountNudge } from "./nudge";

export const mounts: AreaMounts = {
  live: mountLiveSpectrum,
  axis: mountAxis,
  // T-409: the tuning nudges, beside the frequency controls in the top bar. They move the tuned
  // centre — a device action — so they belong to the centre area and go through `view.ts`'s gate.
  nudge: mountNudge,
  // T-340: one edge navigator parallel to each waterfall axis.
  ...navigatorMounts,
};
