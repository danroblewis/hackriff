// MUI entry (ADR-0013, T-156): served at / (dist/index.html), also aliased at /app.html.
// Builds the store and the shell, then mounts every area's `mounts` table (`<area>/index.ts`).
// Panel tasks edit their own area's index.ts, never this file.
import { ControlClient } from "../controls/client";
import * as capture from "./capture";
import * as centre from "./centre";
import type { AppContext, AreaMounts } from "./context";
import * as decode from "./decode";
import * as dock from "./dock";
import { slot } from "./dom";
import * as explore from "./explore";
import { forgetToken, takeToken } from "./net";
import * as review from "./review";
import { PREFS_KEY, mountShell } from "./shell";
import { createStore } from "./store";
import { initialState, parsePrefs } from "./state";

const AREAS: readonly AreaMounts[] = [explore.mounts, centre.mounts, capture.mounts, decode.mounts, dock.mounts, review.mounts];

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
  for (const area of AREAS) {
    for (const [name, mount] of Object.entries(area)) mount(slot(name), ctx);
  }
}

main();
