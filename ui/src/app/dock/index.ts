// Outputs dock mount (ADR-0013 §8). Owner: T-150.
import type { AreaMounts } from "../context";
import { placeholder } from "../placeholder";

export const mounts: AreaMounts = {
  outputs: placeholder("Outputs", "T-150", "Nothing streaming. Listen or run a decode pipeline to add one."),
};
