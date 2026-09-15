// What every MUI panel's `mount(el, ctx)` receives (ADR-0013 §2). Panels talk to the backend only
// through `client` (docs/api.md routes) and `net.openStream` (streams), and to each other only
// through `store`.
import type { ControlClient } from "../controls/client";
import type { Store } from "./store";
import type { AppState } from "./state";

export interface AppContext {
  store: Store<AppState>;
  client: ControlClient;
  token: string;
}

/** A panel entry point. Owns everything inside `el`; never touches DOM outside it. */
export type MountFn = (el: HTMLElement, ctx: AppContext) => void;
