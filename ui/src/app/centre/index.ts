// Centre live view mounts (ADR-0013 §8). Owner: T-152.
import type { AreaMounts } from "../context";
import { mountAxis } from "./axis-view";
import { mountLiveSpectrum } from "./live-spectrum";
import { navigatorMounts } from "./navigators";

export const mounts: AreaMounts = {
  live: mountLiveSpectrum,
  axis: mountAxis,
  // T-340: one edge navigator parallel to each waterfall axis.
  ...navigatorMounts,
};
