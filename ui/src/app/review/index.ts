// Review drawer mount (ADR-0013 §8). Owner: T-155.
import type { AreaMounts } from "../context";
import { mountReview } from "./drawer";

export const mounts: AreaMounts = {
  review: mountReview,
};
