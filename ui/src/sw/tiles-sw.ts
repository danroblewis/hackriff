// The Service Worker that gives a reload an instant first paint offline (T-1039).
//
// Built to `dist/sw-tiles.js` (`ui/package.json`'s `build:sw`) and registered from `../app/main.ts`
// at the page's own origin, so its scope covers every request the app makes. All the logic that can
// be tested without a real worker environment lives in `./tile-cache-logic.ts`, which
// `ui/test/sw-tile-cache.test.ts` exercises directly; this file is the thin `fetch`-event wiring
// around it, and is deliberately kept that way — a `ServiceWorkerGlobalScope` cannot be constructed
// in `node:test`.
//
// **What it does, and does not do.** It intercepts `GET /api/tiles/batch` — the route every
// production client actually asks through (T-573) — and nothing else: a single-tile `GET
// /api/tiles` passes straight through, because T-574 already gives a sealed one an `ETag` and
// `Cache-Control: immutable`, which the browser's own HTTP cache honours without any help from here.
// On a successful batch answer it files away the SEALED entries only (never a live one — see
// `./tile-cache-logic.ts`'s header comment for why that is the whole correctness argument) under a
// synthetic key nothing else can reach. On a failed one — the network is down, or the origin is
// unreachable — it reconstructs the same `{tiles, remaining}` shape from whatever sealed tiles it has
// cached for the requested addresses, so `../surface/tilebatch.ts` sees an ordinary partial answer
// and the canvas paints what it can rather than nothing.

import {
  SEALED_CACHE_NAME, buildOfflineBatch, parseBatchUrl, sealedCacheUrl, sealedEntries,
  type BatchEntryLike, type ParsedBatchRequest,
} from "./tile-cache-logic";

// **No `lib.webworker.d.ts` reference** (deliberately): the project's one `tsconfig.json` already
// includes `"dom"` for every other file under `src/`, and `dom`/`webworker` declare incompatible
// globals (`self` chief among them) — pulling in the full worker lib here would either conflict or
// force a second tsconfig this repo has no build step for. So the handful of worker-only shapes this
// file actually touches are declared locally, minimal and structural, exactly like [[TileFetch]] in
// `../surface/tile.ts` types `fetch` without importing the whole `lib.dom` fetch surface.
interface SwCache {
  match(key: string): Promise<{ json(): Promise<unknown> } | undefined>;
  put(key: string, response: Response): Promise<void>;
}
interface SwGlobal {
  addEventListener(type: "install", listener: () => void): void;
  addEventListener(type: "activate", listener: (event: { waitUntil(p: Promise<unknown>): void }) => void): void;
  addEventListener(type: "fetch", listener: (event: {
    request: Request; respondWith(p: Promise<Response>): void;
  }) => void): void;
  skipWaiting(): Promise<void>;
  clients: { claim(): Promise<void> };
  caches: { open(name: string): Promise<SwCache> };
}
declare const self: SwGlobal;

// Take over immediately: a stale worker still controlling the page after a deploy would answer an
// old build's requests, and this cache's contract (sealed bytes never change) makes that harmless
// either way — there is nothing to gain by waiting out the old worker's lifetime.
self.addEventListener("install", () => {
  void self.skipWaiting();
});
self.addEventListener("activate", (event) => {
  event.waitUntil(self.clients.claim());
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;
  const parsed = parseBatchUrl(req.url);
  if (!parsed) return; // not a batch request: let the browser handle it (and its own HTTP cache).
  event.respondWith(handleBatch(req, parsed));
});

async function handleBatch(req: Request, parsed: ParsedBatchRequest): Promise<Response> {
  const cache = await self.caches.open(SEALED_CACHE_NAME);
  try {
    const res = await fetch(req);
    // Best-effort: filing the sealed entries away must never be why the page's own answer is late
    // or missing, so it runs on a CLONE and its own failure (an unparsable body) is swallowed.
    if (res.ok) void res.clone().json().then((body) => storeSealed(cache, parsed, body)).catch(() => {});
    return res;
  } catch (err) {
    const offline = await reconstructOffline(cache, parsed);
    if (!offline) throw err; // nothing cached for any of it — there is no honest answer to give.
    return new Response(JSON.stringify(offline), {
      status: 200, headers: { "Content-Type": "application/json" },
    });
  }
}

async function storeSealed(cache: SwCache, parsed: ParsedBatchRequest, body: unknown): Promise<void> {
  const tiles = Array.isArray((body as { tiles?: unknown[] } | null)?.tiles)
    ? (body as { tiles: BatchEntryLike[] }).tiles
    : [];
  for (const e of sealedEntries(tiles)) {
    const spelling = e.address!.spelling!;
    await cache.put(
      sealedCacheUrl(parsed, spelling),
      new Response(JSON.stringify(e.tile), { headers: { "Content-Type": "application/json" } }),
    );
  }
}

/** `null` when the cache has NOTHING for any requested address — the caller then re-throws the
 * original network error rather than manufacture an all-`remaining` answer that looks like progress. */
async function reconstructOffline(
  cache: SwCache, parsed: ParsedBatchRequest,
): Promise<{ tiles: BatchEntryLike[]; remaining: string[] } | null> {
  const found = new Map<string, unknown>();
  for (const spelling of parsed.addresses) {
    const hit = await cache.match(sealedCacheUrl(parsed, spelling));
    if (hit) found.set(spelling, await hit.json());
  }
  if (found.size === 0) return null;
  return buildOfflineBatch(parsed.addresses, found);
}
