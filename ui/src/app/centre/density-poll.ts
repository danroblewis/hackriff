// The density layer's HOST side (T-927, the non-blocking follow-ups T-810's re-review found):
// which density tile addresses are asked for, when, over what request, and where a completion is
// allowed to be written.
//
// It lives in its own module — not inline in `./surface.ts`'s wiring — so each of the four rules
// below is asserted directly in `ui/test/surface-density.test.ts` against the behaviour it replaces,
// instead of by regex over a 1400-line host file.
//
// Thin client: the counts are the backend's (`GET /api/tiles/events`), the geometry is
// `../../surface/density.ts`'s, and this file only reads — never a device route, never a clock
// anything is placed at (its pacing is counted in poll ticks, `DENSITY_POLL_MS`, the T-393/T-386
// guard).
//
// The four rules, each a defect T-810 shipped:
//
//  1. **A read has a deadline.** `ApiClient.get` has no timeout, so a density GET that never settles
//     left the pane's in-flight marker set for as long as the browser's own connection timeout, and
//     nothing for that address was ever asked again. Every read here is issued with an
//     [[AbortSignal]] that fires at [[DENSITY_REQUEST_TIMEOUT_MS]] *and* a timer that rejects the
//     wait, so the in-flight record clears and the address goes back onto `DensityFetches`' retry
//     backoff even if the reader ignores its signal.
//  2. **One request per address, stamped with the edge it was ISSUED at.** In-flight is tracked per
//     ADDRESS, not per pane, so a second pane looking at the same tile joins the request in flight
//     rather than duplicating the GET — and the answer is recorded with the live edge of the tick
//     that issued it, never the joining pane's own later tick (which would claim a snapshot
//     seconds fresher than it is, and could seal a tile the edge was still inside).
//  3. **A completion is written against the CURRENT addresses, never the ones it was sent with.**
//     After a pan, a zoom, or the layer being switched off, the old set must not come back for a
//     second; so a completion re-reads what each pane is asking for *now* and writes that.
//  4. (In `../../surface/density.ts`) a non-finite edge is never stored as the fetch edge.
import { DensityFetches, densityAddrs, densityUrl, isCoarseZoom, parseDensityTile, type DensityTile } from "../../surface/density";
import { addrSpelling, type Lattice, type TileAddr } from "../../surface/lattice";
import type { PaneView } from "../../surface/surface";

/** The centre poll's period, ms — the unit the density refresh paces its asks in. */
export const DENSITY_POLL_MS = 1000;

/** How long one density read may take before it is abandoned and retried. Well above a healthy
 * answer and well below the browser's own connection timeout: the point is that a hung GET releases
 * its address on a bound this code knows, not one the network chooses. */
export const DENSITY_REQUEST_TIMEOUT_MS = 10_000;

/** Reads one density URL. `signal` aborts it at the deadline. */
export type DensityReader = (url: string, signal: AbortSignal) => Promise<unknown>;

/**
 * Per-pane density tiles, and the one place that decides when an address is asked for.
 *
 * `tick` is the poll: it drops panes that are gone, gives every density-on, coarse-zoomed pane the
 * copies in hand, and issues the reads `DensityFetches.due` asks for that are not already in flight.
 * `tilesFor` is what the frame draws with — read per frame, never written there.
 */
export class DensityPoll {
  private readonly fetches = new DensityFetches();
  /** Addresses being read right now -> the live edge the read was issued at (rule 2). */
  private readonly inflight = new Map<string, number>();
  /** What each pane is asking for as of the last tick — the set a completion writes against (rule 3). */
  private readonly addrsByPane = new Map<string, readonly TileAddr[]>();
  private readonly byPane = new Map<string, DensityTile[]>();
  private polls = 0;

  constructor(
    private readonly read: DensityReader,
    private readonly timeoutMs: number = DENSITY_REQUEST_TIMEOUT_MS,
  ) {}

  /** The tiles pane `id` draws with — `[]` for a pane with the layer off or no answer yet. */
  tilesFor(id: string): readonly DensityTile[] { return this.byPane.get(id) ?? []; }

  /** The addresses currently being read (one entry per address, however many panes want it). */
  get inflightAddrs(): readonly string[] { return [...this.inflight.keys()]; }

  /**
   * One poll. `on` is the pane's layer switch, `following` the pane's own follow state (a frozen
   * pane is a view over the past and is not revalidated — `DensityFetches`' rule).
   */
  tick(
    lat: Lattice, panes: readonly PaneView[], edgeNs: number,
    on: (id: string) => boolean, following: (id: string) => boolean,
  ): void {
    const ids = new Set(panes.map((p) => p.id));
    for (const id of [...this.byPane.keys()]) if (!ids.has(id)) this.byPane.delete(id);
    for (const id of [...this.addrsByPane.keys()]) if (!ids.has(id)) this.addrsByPane.delete(id);
    const nowMs = ++this.polls * DENSITY_POLL_MS;
    const keep = new Set<string>();
    for (const pane of panes) {
      if (!(on(pane.id) && isCoarseZoom(lat, pane.box, pane.rect.w, pane.rect.h))) {
        this.byPane.set(pane.id, []);
        this.addrsByPane.delete(pane.id);
        continue;
      }
      const addrs = densityAddrs(lat, pane.box, pane.rect.w, pane.rect.h, pane.device ?? "any");
      for (const a of addrs) keep.add(addrSpelling(a));
      this.addrsByPane.set(pane.id, addrs);
      this.byPane.set(pane.id, this.fetches.tiles(addrs));
      for (const a of this.fetches.due(lat, addrs, edgeNs, following(pane.id), nowMs)) {
        // Already being read: this pane JOINS that read, and the edge it was issued at stands.
        if (this.inflight.has(addrSpelling(a))) continue;
        this.issue(a, edgeNs, nowMs);
      }
    }
    this.fetches.retain(keep);
  }

  /** Issue one read, with its deadline; record the answer against the edge of THIS tick. */
  private issue(a: TileAddr, edgeAtIssueNs: number, nowMs: number): void {
    const key = addrSpelling(a);
    this.inflight.set(key, edgeAtIssueNs);
    const ac = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const deadline = new Promise<never>((_resolve, reject) => {
      timer = setTimeout(() => {
        ac.abort();
        reject(new Error(`density read timed out after ${this.timeoutMs} ms: ${key}`));
      }, this.timeoutMs);
    });
    void Promise.race([this.read(densityUrl(a), ac.signal), deadline])
      .then((body) => {
        const t = parseDensityTile(a, body);
        if (t) this.fetches.succeeded(a, t, edgeAtIssueNs, nowMs);
        else this.fetches.failed(a, nowMs);
      })
      .catch(() => { this.fetches.failed(a, nowMs); })
      .finally(() => {
        if (timer !== undefined) clearTimeout(timer);
        this.inflight.delete(key);
        // Written against what the panes ask for NOW, never the addresses this read was sent with.
        for (const [id, addrs] of this.addrsByPane) this.byPane.set(id, this.fetches.tiles(addrs));
      });
  }
}
