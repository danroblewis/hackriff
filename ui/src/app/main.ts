// MUI entry (ADR-0013): served as /app.html beside the old stacked UI (/) until T-156 swaps them.
// Builds the store, the shell, and mounts one module per `data-slot`. Each MUI task (T-150..T-155)
// replaces its placeholder import below with its panel module; that import line is the only
// shared edit point.
import { ControlClient } from "../controls/client";
import { mountLiveSpectrum } from "./centre/live-spectrum";
import type { AppContext, MountFn } from "./context";
import { slot } from "./dom";
import { forgetToken, takeToken } from "./net";
import { placeholder } from "./placeholder";
import { PREFS_KEY, mountShell } from "./shell";
import { createStore } from "./store";
import { initialState, parsePrefs } from "./state";

const MOUNTS: readonly [string, MountFn][] = [
  // Explore
  ["inventory", placeholder("Signal inventory", "T-151", "Candidates and Confirmed.")],
  ["selections", placeholder("Selections", "T-151", "Drag on the waterfall to mark a region.")],
  ["live", mountLiveSpectrum], // T-152
  ["axis", placeholder("", "T-152")],
  ["capture", placeholder("Capture", "T-150", "Always-on capture timeline.")],
  ["focus", placeholder("Focus", "T-151", "Select a signal.")],
  // Decode
  ["pipelines", placeholder("Pipelines", "T-153")],
  ["stages", placeholder("Stages", "T-153")],
  ["plots", placeholder("Stage plots", "T-153")],
  ["inspector", placeholder("Output stream", "T-154", "Frames, bytes and fields.")],
  ["params", placeholder("Block parameters", "T-153")],
  // Both modes
  ["outputs", placeholder("Outputs", "T-150", "Nothing streaming. Listen or run a decode pipeline to add one.")],
  ["review", placeholder("Review", "T-155", "Alarms, survey report, scheduler, device controls.")],
];

function readPrefs(): string | null {
  try { return localStorage.getItem(PREFS_KEY); } catch { return null; }
}

function main() {
  const token = takeToken();
  const store = createStore(initialState(parsePrefs(readPrefs())));
  const ctx: AppContext = { store, client: new ControlClient(token ?? ""), token: token ?? "" };
  mountShell(ctx);
  if (!token) {
    forgetToken();
    store.set((s) => ({ conn: { ...s.conn, api: "unauthorized", message: "token needed" } }));
    return;
  }
  for (const [name, mount] of MOUNTS) mount(slot(name), ctx);
}

main();
