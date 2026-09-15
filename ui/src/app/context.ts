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

/** What each area's `index.ts` exports as `mounts`: `data-slot` name → mount. main.ts mounts every
 * area's table; a slot appears in exactly one area. */
export type AreaMounts = Readonly<Record<string, MountFn>>;
