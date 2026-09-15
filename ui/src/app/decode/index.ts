// Decode workbench mounts (ADR-0013 §8). Owner: T-153. The `inspector` slot's mount lives in
// T-154's `decode/inspector.ts`; T-153 never edits that import line, T-154 never edits this file.
import type { AreaMounts } from "../context";
import { mountInspector } from "./inspector";
import { mountParams } from "./params";
import { mountPipelines } from "./pipelines";
import { mountPlots } from "./plots";
import { mountStages } from "./stages";

export const mounts: AreaMounts = {
  pipelines: mountPipelines,
  stages: mountStages,
  plots: mountPlots,
  params: mountParams,
  inspector: mountInspector,
};
