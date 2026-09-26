// MUI entry (ADR-0013, T-156; T-179 code-splitting): served at / (dist/index.html), also aliased
// at /app.html. Builds the store and the shell, then mounts every area's `mounts` table
// (`<area>/index.ts`). Panel tasks edit their own area's index.ts, never this file.
//
// Bundle budget (ADR-0013 §1, ≤45 KB gzip): explore/centre/dock mount eagerly, because
// they're the initial Explore screen (must work with no network round-trip beyond the app itself).
// decode and review are the two rarely-used areas (Decode workbench, Review drawer) — each is
// loaded with a dynamic `import()` the first time it's needed (first switch to Decode mode, first
// Review-drawer open) so esbuild's `--splitting` puts their code in separate chunks that never
// load for a session that stays in Explore. `--format=esm` is required for splitting, so
// `index.html`'s `<script>` is `type="module"`.
import { ControlClient } from "../controls/client";
import * as centre from "./centre";
import type { AppContext, AreaMounts } from "./context";
import * as dock from "./dock";
import { slot } from "./dom";
import * as explore from "./explore";
import { forgetToken, reloadOnTokenHash, takeToken } from "./net";
import { PREFS_KEY, mountShell } from "./shell";
import { createStore } from "./store";
import { initialState, parsePrefs } from "./state";

const EAGER_AREAS: readonly AreaMounts[] = [explore.mounts, centre.mounts, dock.mounts];

/**
 * **The sealed-tile cache, for an instant first paint offline** (T-1039, `../sw/tiles-sw.ts`).
 *
 * Best-effort and silent: a browser with no Service Worker support, or one that refuses the
 * registration, still runs exactly as before — this is resilience on top of the ordinary network
 * path (`../surface/tilecache.ts`'s own stale-while-revalidate and jittered backoff), never a
 * dependency of it.
 */
function registerTileServiceWorker(): void {
  if (!("serviceWorker" in navigator)) return;
  void navigator.serviceWorker.register("sw-tiles.js").catch(() => {});
}

function mountArea(area: AreaMounts, ctx: AppContext) {
  for (const [name, mount] of Object.entries(area)) mount(slot(name), ctx);
}

function readPrefs(): string | null {
  try { return localStorage.getItem(PREFS_KEY); } catch { return null; }
}

function main() {
  registerTileServiceWorker();
  reloadOnTokenHash(window, sessionStorage);
  const token = takeToken();
  const store = createStore(initialState(parsePrefs(readPrefs())));
  const ctx: AppContext = { store, client: new ControlClient(token ?? ""), token: token ?? "" };
  mountShell(ctx);
  if (!token) {
    forgetToken();
    store.set((s) => ({ conn: { ...s.conn, api: "unauthorized", message: "token needed" } }));
    return;
  }
  for (const area of EAGER_AREAS) mountArea(area, ctx);

  // Decode workbench: mounted on first switch to Decode mode (also fires immediately if a
  // persisted pref restores `mode: "decode"`).
  let decodeLoaded = false;
  store.select((s) => s.mode, (mode) => {
    if (mode !== "decode" || decodeLoaded) return;
    decodeLoaded = true;
    import("./decode").then((m) => mountArea(m.mounts, ctx));
  }, { immediate: true });

  // History surface (T-264): mounted on first switch to History mode, like Decode — the durable
  // catalogue is a deliberate visit, not part of the initial Explore screen.
  let historyLoaded = false;
  store.select((s) => s.mode, (mode) => {
    if (mode !== "history" || historyLoaded) return;
    historyLoaded = true;
    import("./history").then((m) => mountArea(m.mounts, ctx));
  }, { immediate: true });

  // Review drawer: mounted on first open.
  let reviewLoaded = false;
  store.select((s) => s.review.open, (open) => {
    if (!open || reviewLoaded) return;
    reviewLoaded = true;
    import("./review").then((m) => mountArea(m.mounts, ctx));
  }, { immediate: true });
}

main();
