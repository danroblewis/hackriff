// MUI application state (ADR-0013 §3). Composition only: every slice (type, initial value, pure
// actions) lives in its owning area's `slice.ts`, so panel tasks never edit this file. Every slice
// is plain data from docs/api.md (or UI-only interaction state); no slice holds a derived signal
// measurement computed in the browser. Actions are pure `(state) => patch` functions.
import { analyzeInitial, type AnalyzeState } from "./explore/analyze-slice";
import { captureInitial, type CaptureState } from "./centre/capture-slice";
import { centreInitial, type CentreState } from "./centre/slice";
import { inspectorInitial, type InspectorState } from "./decode/inspector-slice";
import { decodeInitial, type DecodeState } from "./decode/slice";
import { dockInitial, type DockState } from "./dock/slice";
import { exploreInitial, type ExploreState } from "./explore/slice";
import { reviewInitial, type ReviewState } from "./review/slice";
import { layersInitial, type LayersState } from "./map/layers-slice";
import { parsePrefs, shellInitial, type Prefs, type ShellState } from "./shell-slice";

export * from "./shell-slice";
export * from "./centre/capture-slice";
export * from "./dock/slice";
export * from "./explore/slice";
export * from "./centre/slice";
export * from "./decode/slice";
export * from "./decode/inspector-slice";
export * from "./review/slice";
export * from "./explore/analyze-slice";
export * from "./map/layers-slice";

export interface AppState extends ShellState, CaptureState, DockState, ExploreState, CentreState, DecodeState, InspectorState, ReviewState, AnalyzeState, LayersState {}

export function initialState(prefs: Prefs = parsePrefs(null)): AppState {
  return {
    ...shellInitial(prefs), ...captureInitial(), ...dockInitial(), ...exploreInitial(),
    ...centreInitial(), ...decodeInitial(), ...inspectorInitial(), ...reviewInitial(), ...analyzeInitial(), ...layersInitial(),
  };
}
