// The density layer's HOST side (T-927, the non-blocking follow-ups T-810's re-review found):
// which density tile addresses are asked for, when, over what request, and where a completion is
// allowed to be written.
//
// It lives in its own module — not inline in `./surface.ts`'s wiring — so each rule below is
// asserted directly in `ui/test/surface-density.test.ts` against the behaviour it replaces, instead
// of by regex over a 1400-line host file.
//
// The policy is `../../surface/density.ts`'s and unchanged: per-pane copies, every tile a FOLLOWING
// pane shows re-asked as rows arrive (counts are back-dated to an event's start, so no tile is ever
// sealed), a frozen pane asking once, failures retried with backoff, one request per address in
// flight shared between panes (`sharedDensityReader`). What this module adds is the three things
// T-810 left open:
//
//  1. **A read has a deadline.** `ControlClient.get` has no timeout, so a density GET that never
//     settled left the pane's in-flight marker set for as long as the browser's own connection
//     timeout — and, because the shared reader keys on the address, it stalled every other pane
//     asking for that address too. Every read here is issued with an `AbortSignal` that fires at
//     [[DENSITY_REQUEST_TIMEOUT_MS]] *and* a timer that rejects the wait, so the address is released
//     and goes back onto `DensityFetches`' retry backoff even if the reader ignores its signal.
//  2. **A joined request is recorded at the edge it was ISSUED at**, never at the joining pane's own
//     later tick: the shared reader answers with that edge (`DensityAnswer.edgeAtFetchNs`) and this
//     is what `succeeded` is given. A frozen pane joining a following pane's request otherwise kept
//     a copy stamped seconds fresher than the picture in it.
//  3. **A completion is written against the CURRENT addresses**, never the ones it was sent with:
//     after a pan, a zoom, or the layer being switched off, the old set must not come back for a
//     second.
//
// (The fourth follow-up — a non-finite edge must not be stored as the fetch edge — is in
// `DensityFetches.succeeded` itself; the fifth is [[DENSITY_POLL_MS]], the one spelling of the
// period, imported by the host's `startPoll`.)
//
// Thin client: the counts are the backend's (`GET /api/tiles/events`), the geometry is
// `../../surface/density.ts`'s, and this module only reads — never a device route, and never a
// browser clock (its pacing is counted in poll ticks, the T-393/T-386 guard).
import {
  DensityFetches, densityAddrs, isCoarseZoom, sharedDensityReader, type DensityAnswer, type DensityTile,
} from "../../surface/density";
import type { Lattice, TileAddr } from "../../surface/lattice";
import type { PaneView } from "../../surface/surface";

/** The centre poll's period, ms — the unit the density refresh paces its asks in. */
export const DENSITY_POLL_MS = 1000;

/** How long one density read may take before it is abandoned and retried. Well above a healthy
 * answer and well below the browser's own connection timeout: the point is that a hung GET releases
 * its address on a bound this code knows, not one the network chooses. */
export const DENSITY_REQUEST_TIMEOUT_MS = 10_000;

/** The host's read: a URL and the deadline's signal in, the response body out. */
export type DensityGet = (url: string, signal: AbortSignal) => Promise<unknown>;

/** `get` raced against its deadline: at `ms` the request is aborted AND the wait rejects, so a
 * reader that ignores its signal cannot hold the address either. */
function withDeadline(get: (signal: AbortSignal) => Promise<unknown>, ms: number, what: string): Promise<unknown> {
  const ac = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const deadline = new Promise<never>((_resolve, reject) => {
    timer = setTimeout(() => {
      ac.abort();
      reject(new Error(`density read timed out after ${ms} ms: ${what}`));
    }, ms);
  });
  return Promise.race([get(ac.signal), deadline]).finally(() => { if (timer !== undefined) clearTimeout(timer); });
}

/**
 * Per-pane density tiles, and the one place that decides when an address is asked for.
 *
 * `tick` is the poll: it drops panes that are gone, gives every density-on, coarse-zoomed pane the
 * copies in hand, and issues the reads `DensityFetches.due` asks for. `tilesFor` is what the frame
 * draws with — read per frame, never written there.
 */
export class DensityPoll {
  private readonly fetches = new DensityFetches();
  private readonly read: (a: TileAddr, edgeNs: number) => Promise<DensityAnswer>;
  /** Panes with a batch in flight (the reviewed rule: one batch per pane). */
  private readonly inflight = new Set<string>();
  /** What each pane is asking for as of the last tick — the set a completion writes against (3). */
  private readonly addrsByPane = new Map<string, readonly TileAddr[]>();
  private readonly byPane = new Map<string, DensityTile[]>();
  private polls = 0;

  constructor(get: DensityGet, timeoutMs: number = DENSITY_REQUEST_TIMEOUT_MS) {
    this.read = sharedDensityReader((url) => withDeadline((signal) => get(url, signal), timeoutMs, url));
  }

  /** The tiles pane `id` draws with — `[]` for a pane with the layer off or no answer yet. */
  tilesFor(id: string): readonly DensityTile[] { return this.byPane.get(id) ?? []; }

  /** The panes with a batch in flight (a hung read must not leave one here). */
  get inflightPanes(): readonly string[] { return [...this.inflight]; }

  /**
   * One poll. `on` is the pane's layer switch, `following` its own follow state (a frozen pane is a
   * view over the past: it asks once and keeps its own copy — `DensityFetches`' rule).
   */
  tick(
    lat: Lattice, panes: readonly PaneView[], edgeNs: number,
    on: (id: string) => boolean, following: (id: string) => boolean,
  ): void {
    const ids = new Set(panes.map((p) => p.id));
    for (const id of [...this.byPane.keys()]) if (!ids.has(id)) this.byPane.delete(id);
    for (const id of [...this.addrsByPane.keys()]) if (!ids.has(id)) this.addrsByPane.delete(id);
    const nowMs = ++this.polls * DENSITY_POLL_MS;
    const keep: [string, TileAddr][] = [];
    for (const pane of panes) {
      if (!(on(pane.id) && isCoarseZoom(lat, pane.box, pane.rect.w, pane.rect.h))) {
        this.byPane.set(pane.id, []);
        this.addrsByPane.delete(pane.id);
        continue;
      }
      const addrs = densityAddrs(lat, pane.box, pane.rect.w, pane.rect.h, pane.device ?? "any");
      for (const a of addrs) keep.push([pane.id, a]);
      this.addrsByPane.set(pane.id, addrs);
      this.byPane.set(pane.id, this.fetches.tiles(pane.id, addrs));
      if (this.inflight.has(pane.id)) continue;
      const due = this.fetches.due(pane.id, addrs, edgeNs, following(pane.id), nowMs);
      if (due.length === 0) continue;
      const id = pane.id;
      this.inflight.add(id);
      void Promise.all(due.map((a) => this.read(a, edgeNs).then((ans) => {
        // The edge the request was ISSUED at, which for a joined request is not `edgeNs` here.
        if (ans.tile) this.fetches.succeeded(id, a, ans.tile, ans.edgeAtFetchNs, nowMs);
        else this.fetches.failed(id, a, nowMs);
      })))
        .finally(() => {
          this.inflight.delete(id);
          // Written against what each pane asks for NOW, never the addresses this batch was sent with.
          for (const [paneId, current] of this.addrsByPane) this.byPane.set(paneId, this.fetches.tiles(paneId, current));
        });
    }
    this.fetches.retain(keep);
  }
}
