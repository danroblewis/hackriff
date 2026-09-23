// **One request per viewport, not one per tile** (T-573, docs/api.md "GET /api/tiles/batch").
//
// `TileCache` asks for exactly one address at a time — deliberately: its slot budget, its
// per-address abort, its live-edge revalidation lane and its per-viewport fairness are all
// per-tile facts, and folding batching into that scheduler would mean re-deriving every one of
// them. So batching lives HERE, as a source adapter: the cache still calls
// `(addr, signal) => Promise<TileData>` and gets one tile back, and this file coalesces the calls
// the cache makes in one turn into one HTTP request.
//
// **Why a turn is the right window.** `TileCache.pump()` issues up to its whole budget in one
// synchronous loop, so every request a render frame wants is already in hand by the end of that
// turn. A microtask is therefore not a delay heuristic tuned to a cadence — it is the earliest
// moment the set is complete, and waiting longer would only add latency to the live edge.
//
// **Abort still belongs to the address.** A batch is only abandoned when EVERY member of it has
// been abandoned; one viewport moving away must not cancel a tile another viewport is waiting on.
//
// **A partial answer stays partial.** Each entry carries its own status, so a 503 rejects only its
// own address (as `TileBusyError`, which the cache already knows how to back off from) and the
// tiles beside it resolve. `truncated` puts the unanswered addresses back in the queue rather than
// failing them: the route said it did not reach them, not that they are unreachable.

import { errorFrom } from "../controls/client";
import { buildRequest } from "../controls/client";
import {
  TILES_BATCH_MAX_ADDRESSES, addrSpelling, batchGroupKey, tilesBatchUrl,
  type TileAddr,
} from "./lattice";
import {
  TileBusyError, TileDecodeError, capFromRefusal, decodeTile, fetchTile,
  type TileData, type TileFetch, type TileResponse,
} from "./tile";
import type { TileSourceHint } from "./tilecache";

/** One waiting address. */
interface Waiter {
  readonly addr: TileAddr;
  readonly signal?: AbortSignal;
  readonly resolve: (d: TileData) => void;
  readonly reject: (e: unknown) => void;
}

/** One entry of a `GET /api/tiles/batch` answer. */
interface BatchEntry {
  address?: { spelling?: string };
  status?: number;
  error?: string;
  tile?: TileResponse;
}

/** The batch answer's own shape. */
interface BatchResponse {
  tiles?: BatchEntry[];
  truncated?: boolean;
  remaining?: string[];
}

/**
 * A `TileCache` source that answers one address at a time but ASKS in batches.
 *
 * Drop-in for `(a, signal) => fetchTile(a, token, fetchFn, signal)`.
 */
export function batchedTileSource(
  token: string,
  fetchFn: TileFetch,
  opts: { readonly max?: number; readonly schedule?: (fn: () => void) => void } = {},
): (addr: TileAddr, signal?: AbortSignal, hint?: TileSourceHint) => Promise<TileData> {
  const max = Math.max(1, Math.min(opts.max ?? TILES_BATCH_MAX_ADDRESSES, TILES_BATCH_MAX_ADDRESSES));
  const schedule = opts.schedule
    ?? ((fn: () => void) => { void Promise.resolve().then(fn); });
  const pending = new Map<string, Waiter[]>();
  const armed = new Set<string>();

  const flush = (group: string): void => {
    armed.delete(group);
    const queue = pending.get(group);
    if (!queue || queue.length === 0) { pending.delete(group); return; }
    // Anything already abandoned never reaches the wire. Rejecting it is what the single-address
    // path does too (`fetch` rejects on an aborted signal), so the cache sees no new shape.
    const live = queue.filter((w) => {
      if (!w.signal?.aborted) return true;
      w.reject(abortError());
      return false;
    });
    const batch = live.slice(0, max);
    const rest = live.slice(max);
    if (rest.length) { pending.set(group, rest); arm(group); } else { pending.delete(group); }
    if (batch.length === 0) return;
    void issue(batch, group);
  };

  const arm = (group: string): void => {
    if (armed.has(group)) return;
    armed.add(group);
    schedule(() => flush(group));
  };

  const requeue = (w: Waiter): void => {
    const group = batchGroupKey(w.addr);
    const q = pending.get(group);
    if (q) q.push(w); else pending.set(group, [w]);
    arm(group);
  };

  async function issue(batch: Waiter[], group: string): Promise<void> {
    // One controller for the batch, aborted only when every member has been abandoned.
    const ctrl = typeof AbortController === "function" ? new AbortController() : null;
    let remainingLive = batch.length;
    const onAbort = (): void => { if (--remainingLive <= 0) ctrl?.abort(); };
    for (const w of batch) w.signal?.addEventListener?.("abort", onAbort, { once: true });

    const req = buildRequest("GET", tilesBatchUrl(batch.map((w) => w.addr)), token);
    let body: BatchResponse;
    try {
      const r = await fetchFn(req.url, { ...req.init, signal: ctrl?.signal });
      const parsed = await r.json().catch(() => ({}));
      if (!r.ok) throw errorFrom(r.status, parsed, r.statusText);
      body = parsed as BatchResponse;
    } catch (e) {
      // The transport failed for the whole request, so it failed for every address in it. Each
      // waiter is rejected in its own right: the cache's retry and backoff are per address.
      for (const w of batch) w.reject(e);
      return;
    }

    const byAddress = new Map<string, BatchEntry>();
    for (const e of body.tiles ?? []) {
      if (typeof e.address?.spelling === "string") byAddress.set(e.address.spelling, e);
    }
    const unreached = new Set(body.remaining ?? []);
    for (const w of batch) {
      const spelling = addrSpelling(w.addr);
      if (unreached.has(spelling)) { requeue(w); continue; }
      const e = byAddress.get(spelling);
      if (!e) {
        // The route answered without this address and without listing it as unreached. That is
        // not a coverage answer, so it throws rather than resolving to anything: the place stays
        // pending, which is true, instead of claiming the radio never looked.
        w.reject(new TileDecodeError(
          `batch answer named no entry for ${spelling}: an address that is neither answered nor listed in \`remaining\` is a tile left pending with nothing saying why`));
        continue;
      }
      const status = e.status ?? 0;
      if (status !== 200 || !e.tile) {
        const err = errorFrom(status || 502, { error: e.error }, e.error ?? "tile refused");
        w.reject(status === 503 ? new TileBusyError(capFromRefusal(err.message), err.message) : err);
        continue;
      }
      try { w.resolve(decodeTile(w.addr, e.tile)); } catch (err) { w.reject(err); }
    }
    void group;
  }

  return (addr, signal, hint) => {
    // **The live edge rides alone.** A batch answers when its slowest member does, so a
    // revalidation coalesced with cold tiles would hold the newest rows until those were built —
    // the live edge gated on tile generation, which the product forbids. It goes out as the
    // single-tile request it always was.
    if (hint?.solo) return fetchTile(addr, token, fetchFn, signal);
    return new Promise<TileData>((resolve, reject) => {
      if (signal?.aborted) { reject(abortError()); return; }
      requeue({ addr, signal, resolve, reject });
    });
  };
}

/** What `fetch` rejects with on an aborted signal, so the cache's own abort test still holds. */
function abortError(): Error {
  const e = new Error("aborted");
  e.name = "AbortError";
  return e;
}
