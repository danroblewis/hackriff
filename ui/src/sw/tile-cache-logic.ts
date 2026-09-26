// Pure logic behind the sealed-tile Service Worker cache (T-1039, `./tiles-sw.ts`). Split out of the
// worker itself so it is testable with plain `node:test` — no `ServiceWorkerGlobalScope`, no
// `caches`, no fetch event, just the parsing/splitting/reconstruction the worker calls.
//
// **What this exists to do, in one sentence.** `GET /api/tiles/batch` — the route every production
// client actually uses (`../surface/tilebatch.ts`, T-573) — carries "No ETag, no immutable cache"
// (docs/api.md §"Caching (T-574)"): a batch is not one representation, so the browser's own HTTP
// cache cannot help it the way it already helps a single sealed `/api/tiles` read. Without something
// keeping a copy, a reload with the server unreachable has nothing to paint at all, however much of
// the last viewport was sealed and immutable a moment before.
//
// **Sealed tiles only, on purpose, and it is the whole correctness argument** (the same one T-574
// makes one route up): a sealed tile's own time extent has fully passed the pyramid's watermark, so
// it can never change again — caching it is free. A live tile at the growing edge changes on every
// arriving row; caching one here would let a stale copy answer a page that reloaded expecting the
// edge to keep moving, which is exactly the "we have it but did not render it" defect with a new
// cause. So [[sealedEntries]] is the one gate every stored byte passes through, and nothing else in
// this file, or in `./tiles-sw.ts`, may store a tile that did not pass it.

/** One entry of a `GET /api/tiles/batch` answer, structurally — only the fields this file reads. */
export interface BatchEntryLike {
  readonly address?: { readonly spelling?: string };
  readonly status?: number;
  readonly tile?: { readonly sealed?: boolean; readonly [key: string]: unknown };
}

/** What this cache needs out of a batch request's own URL. */
export interface ParsedBatchRequest {
  readonly device: string;
  readonly scheme: string;
  readonly cells: number;
  readonly planes: string;
  readonly addresses: readonly string[];
}

/** The Cache Storage name this worker owns. Versioned so a future change to the stored shape can
 * start clean rather than trying to read an old body under a new contract. */
export const SEALED_CACHE_NAME = "hk-sealed-tiles-v1";

/**
 * Read `device`/`scheme`/`cells`/`planes`/`addresses` off a `/api/tiles/batch` request URL
 * ([[tilesBatchUrl]] in `../surface/lattice.ts` is what builds it), or `null` for anything else —
 * including a single-tile `/api/tiles` request, which this worker leaves untouched (see the header
 * comment: the browser's own HTTP cache already has that one).
 */
export function parseBatchUrl(url: string): ParsedBatchRequest | null {
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return null;
  }
  if (!u.pathname.endsWith("/api/tiles/batch")) return null;
  const addresses = (u.searchParams.get("addresses") ?? "").split(",").filter((s) => s.length > 0);
  if (addresses.length === 0) return null;
  const cells = Number(u.searchParams.get("cells") ?? "256");
  return {
    device: u.searchParams.get("device") ?? "any",
    scheme: u.searchParams.get("scheme") ?? "view",
    cells: Number.isFinite(cells) && cells > 0 ? cells : 256,
    planes: u.searchParams.get("planes") ?? "compact",
    addresses,
  };
}

/**
 * The Cache Storage key one sealed tile is stored and looked up under: a **synthetic** URL that is
 * never a real route on this or any server, so a request for it can never reach the wire and a
 * response under it can never be served to the page directly — only ever read back by
 * [[buildOfflineBatch]] and folded into a reconstructed batch answer. Keyed on every axis a batch
 * groups by ([[batchGroupKey]] in `../surface/lattice.ts`) plus the address's own spelling, so two
 * viewports asking in different `cells` or on a different scheme never collide.
 */
export function sealedCacheUrl(
  p: Pick<ParsedBatchRequest, "device" | "scheme" | "cells" | "planes">, spelling: string,
): string {
  const q = new URLSearchParams({
    device: p.device, scheme: p.scheme, cells: String(p.cells), planes: p.planes, addr: spelling,
  });
  return `https://hk-sealed-tile.invalid/tile?${q.toString()}`;
}

/**
 * Which of a batch answer's entries this cache may keep, per the header comment's rule: answered
 * (`status === 200`, a `tile` body), sealed (`tile.sealed === true` — T-574's own fact, computed from
 * the pyramid's watermark, never guessed here), and carrying an address this client can name again.
 * Everything else — a live tile, a refusal, an unreached address — is filtered out and never stored.
 */
export function sealedEntries(entries: readonly BatchEntryLike[]): BatchEntryLike[] {
  return entries.filter((e) =>
    e.status === 200 && !!e.tile && e.tile.sealed === true && typeof e.address?.spelling === "string");
}

/**
 * Reconstruct the exact shape `../surface/tilebatch.ts` already reads (`{tiles, remaining}`) from
 * whatever this cache has for the requested addresses — the offline answer (T-1039).
 *
 * Every address not found goes to `remaining`, which the client already treats as "the route did not
 * reach it, ask again" (T-573's `truncated`/`remaining` handling) rather than as a refusal — which is
 * exactly true here: this cache did not refuse it, it simply never had it (because it was live, or
 * was never fetched, or has since been evicted). That is also what keeps a live/unsealed address from
 * ever being answered offline: it can only ever be cached-then-served if [[sealedEntries]] let it in.
 */
export function buildOfflineBatch(
  addresses: readonly string[], cached: ReadonlyMap<string, unknown>,
): { readonly tiles: BatchEntryLike[]; readonly remaining: string[] } {
  const tiles: BatchEntryLike[] = [];
  const remaining: string[] = [];
  for (const spelling of addresses) {
    const tile = cached.get(spelling);
    if (tile !== undefined) tiles.push({ address: { spelling }, status: 200, tile: tile as BatchEntryLike["tile"] });
    else remaining.push(spelling);
  }
  return { tiles, remaining };
}
