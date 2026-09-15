// Review drawer mount (ADR-0013 §8). Owner: T-155.
import type { AreaMounts } from "../context";
import { placeholder } from "../placeholder";

export const mounts: AreaMounts = {
  review: placeholder("Review", "T-155", "Alarms, survey report, scheduler, device controls."),
};
