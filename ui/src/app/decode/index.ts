// Decode workbench mounts (ADR-0013 §8). Owner: T-153. The `inspector` slot's mount lives in
// T-154's `decode/inspector.ts`; T-153 never edits that import line, T-154 never edits this file.
import type { AreaMounts } from "../context";
import { placeholder } from "../placeholder";
import { mountInspector } from "./inspector";

export const mounts: AreaMounts = {
  pipelines: placeholder("Pipelines", "T-153"),
  stages: placeholder("Stages", "T-153"),
  plots: placeholder("Stage plots", "T-153"),
  params: placeholder("Block parameters", "T-153"),
  inspector: mountInspector,
};
