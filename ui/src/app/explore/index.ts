// Explore sidebar + focus panel mounts (ADR-0013 §8). Owner: T-151, which replaces these
// placeholders with its panel modules (explore/inventory.ts, selections.ts, focus.ts).
import type { AreaMounts } from "../context";
import { placeholder } from "../placeholder";

export const mounts: AreaMounts = {
  inventory: placeholder("Signal inventory", "T-151", "Candidates and Confirmed."),
  selections: placeholder("Selections", "T-151", "Drag on the waterfall to mark a region."),
  focus: placeholder("Focus", "T-151", "Select a signal."),
};
