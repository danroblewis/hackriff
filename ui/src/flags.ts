// **Client feature flags, read from the page's own URL.**
//
// One mechanism, and it is a query parameter, because the alternatives all lie about *when* the flag
// was decided. A build-time define would need `dist` rebuilt to answer a question about one page; a
// stored preference would outlive the session that set it and then explain a picture nobody asked
// for. `?live-ring=1` is visible in the address bar, survives a reload, is per tab, and is exactly
// what an e2e spec (or a user being asked "does this build fix it?") can set.
//
// **`HK_UI_LIVE_RING=1`** is the same switch spelled for a shell: `ui/e2e/harness.mjs`'s `appUrl`
// turns it into this parameter, so a spec run with that variable set and a page opened by hand agree
// on one flag with one name.
//
// A flag here is a **lane** switch, never an honesty switch: it may change which path draws a row,
// and may not change what a mark claims. Anything that would soften a claim is not a flag, it is a
// bug (docs/16 §8.3's tiers, and `cellrule.ts`'s one grey).

/** The flags this client reads. */
export interface Flags {
  /**
   * **Paint each following pane's live edge from the published spectrum rows** (T-1042 / LSR-1,
   * `./surface/livering.ts`) instead of from the pyramid's live-edge tiles. Default **off**.
   */
  readonly liveRing: boolean;
}

/** `1`, `true`, `yes`, `on` and a bare `?live-ring` are on; anything else, including absence, is off. */
function on(q: URLSearchParams, name: string): boolean {
  if (!q.has(name)) return false;
  const v = (q.get(name) ?? "").trim().toLowerCase();
  return v === "" || v === "1" || v === "true" || v === "yes" || v === "on";
}

/**
 * The flags of one URL's query string. Pure, so a test states the search it is asserting about
 * instead of reaching for the document.
 */
export function flagsOf(search: string): Flags {
  const q = new URLSearchParams(search.startsWith("?") ? search.slice(1) : search);
  return { liveRing: on(q, "live-ring") };
}

/** This page's flags. Read once per call; there is no cache to go stale across a navigation. */
export function flags(): Flags {
  return flagsOf(typeof location === "undefined" ? "" : location.search);
}
