// Capture timeline mount (ADR-0013 §8). Owner: T-150.
import type { AreaMounts } from "../context";
import { placeholder } from "../placeholder";

export const mounts: AreaMounts = {
  capture: placeholder("Capture", "T-150", "Always-on capture timeline."),
};
