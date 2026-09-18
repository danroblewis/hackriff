// Selections model for Explore (ADR-0013 §4.2, §4.5, T-151): one shared `SelectionStore` (the old
// page's offline-sync model, T-044/T-052) mirrored into the store's `selections` slice, plus the
// pure "found inside" and Listen-to-all helpers the focus panel uses for a selection.
import { apiBackend, type ApiClient, type Selection, SelectionStore, sortSelections, syncText } from "../../selections";
import { RECORD_KINDS } from "../../outputs";
import { apiErrorText } from "./format";
import type { AppContext } from "../context";
import type { ListenTarget } from "../dock/api";
import { windowEmptyText, type Row, type ViewWindow, type WindowEmptiness } from "./inventory";
import { setSelections } from "./slice";

export { SelectionStore, sortSelections };
export type { Selection };

/** The client surface [[recordSelectionClip]] needs — narrower than the full CRUD [[ApiClient]] the
 * `SelectionStore` backend uses, so a test double only has to implement `post`. */
export interface PostClient { post<T>(path: string, body?: unknown): Promise<T> }

function createSelectionStore(client: ApiClient): SelectionStore {
  return new SelectionStore({ backend: apiBackend(client) });
}

let shared: SelectionStore | null = null;

/**
 * The one `SelectionStore` this page uses, created on first use and mirrored into
 * `state.selections` (list, plus a `syncText` status string). Offline changes flush every 15 s
 * (§3.2). Other Explore panels (the focus panel) call this to reach the same instance rather than
 * creating a second one.
 */
export function selectionStoreFor(ctx: AppContext): SelectionStore {
  if (shared) return shared;
  const store = createSelectionStore(ctx.client);
  shared = store;
  store.subscribe((list) => ctx.store.set(setSelections(list, syncText(store.sync(), list.length))));
  void store.load();
  // `unref` where it exists (node), a no-op where it does not (every browser, where `setInterval`
  // returns a number). T-458: without it this one timer keeps a node process alive for ever, so any
  // unit test that reaches this function hangs instead of failing — which is how a *test* stops
  // being able to see a fault in the code that calls it. The browser behaviour is unchanged.
  (setInterval(() => void store.flush(), 15_000) as unknown as { unref?: () => void }).unref?.();
  return store;
}

// ---- the window the sidebar is a view of (T-386) -------------------------------------------
//
// The selections sidebar used to render `selections.list` whole: every selection the page holds,
// at every frequency, for all time, beside a waterfall showing twenty seconds of one band. That is
// the whole-UI window rule broken in the *widening* direction — the surface looked full by
// answering a bigger question than the one on screen — and it is why this file now owns the split
// rather than the mount.
//
// It is the same shape as T-389's `renderedInventory`: **one** filtered collection, derived once,
// with the list and the centre view's boxes each taking a field of it. `live-spectrum.ts` renders
// `listed` too, so the sidebar and the overlays cannot drift apart into two predicates that happen
// to agree.

/** A selection's frequency extent, and its time extent when it has one (docs/api.md `Selection`). */
type Extent = Pick<Selection, "id" | "f_lo" | "f_hi" | "t_lo" | "t_hi">;

/** The page's selections, split by the window on screen (T-386). */
export interface WindowedSelections<T extends Extent = Selection> {
  /** The ones that are in this (time × frequency) window: what the list and the boxes render. */
  listed: T[];
  /** Held by the page, but at another frequency or another time. Disclosed as a count so a
   * filtered-away selection reads as *elsewhere*, never as *gone*. */
  outside: number;
  /** **Timed** selections that could not be placed because no window is known yet — the third
   * state [[WAITING_FOR_WINDOW]] exists for, kept apart from "outside" so a missing window never
   * reads as a finding about where the selection is. */
  undecidable: number;
}

/**
 * The selections of the current window.
 *
 * Two predicates, and the asymmetry between them is the point:
 *
 * - **Frequency** always applies. A selection overlapping the viewed span is in it (the same
 *   `f_hi >= lo && f_lo <= hi` overlap [[foundInside]] uses); `v === null` is *no view known*, and
 *   then nothing is excluded on frequency rather than everything.
 * - **Time** applies only to a selection that *has* a time extent. An untimed selection is a
 *   frequency-only mark — "this band, whenever" — so it is genuinely in every window its frequency
 *   overlaps, and filtering it out on a time window it never claimed would be the "we have it but
 *   didn't render it" bug, not the fix for it. Only a **timed** selection needs `w`, and with no
 *   window known it is `undecidable` rather than in or out.
 *
 * Pure: no clock, no fetch. The window comes from [[viewWindow]], on the capture clock.
 */
export function selectionsInWindow<T extends Extent>(
  list: readonly T[],
  v: { loHz: number; hiHz: number } | null,
  w: ViewWindow | null,
): WindowedSelections<T> {
  const listed: T[] = [];
  let outside = 0, undecidable = 0;
  for (const s of list) {
    if (v && (s.f_hi < v.loHz || s.f_lo > v.hiHz)) { outside++; continue; }
    const timed = typeof s.t_lo === "number" && typeof s.t_hi === "number";
    if (!timed) { listed.push(s); continue; }
    if (!w) { undecidable++; continue; }
    if (s.t_hi! < w.t0 || s.t_lo! > w.t1) { outside++; continue; }
    listed.push(s);
  }
  return { listed, outside, undecidable };
}

/**
 * What an empty selections list says — and **coverage is deliberately not one of its states**.
 *
 * `windowEmptyText` keeps *nothing was observed here* apart from *nothing is here*, because every
 * other window-scoped surface renders a **measurement** and those are opposite claims about the
 * receiver. A selection is not a measurement: the user drew it. Whether the front end ever sampled
 * this window has nothing to do with whether a region was marked in it, so asking `/api/coverage`
 * and saying "nothing was observed in this window" here would be a coverage claim invented to fill
 * a sentence — the same class of lie the rule exists to stop, pointing the other way.
 *
 * So the shared function is used for exactly the two states it owns and that do apply — an error,
 * and [[WAITING_FOR_WINDOW]] — and the two that are about the user's own set are named here. No
 * fourth vocabulary: the one sentence that is shared is the one shared constant.
 */
export function selectionsEmptyText(split: WindowedSelections<Extent>, error: string | null = null): string {
  // The two states the shared function owns and this surface does not: an error, and *no window
  // known*. Neither branch of `windowEmptyText` reads its two sentences, which is why they are
  // empty here — this surface supplies no coverage claim, by design.
  const v: WindowEmptiness | null = error
    ? { kind: "error", message: error }
    : split.undecidable > 0
      ? { kind: "no-window" }
      : null;
  if (v) return windowEmptyText(v, "", "");
  if (split.outside > 0) {
    const n = split.outside;
    return `${n} selection${n === 1 ? "" : "s"}, none in this window — scrub or tune to where ${n === 1 ? "it was" : "they were"} made.`;
  }
  return "Drag across the waterfall to mark a region.";
}

/** Emitters whose extent overlaps the selection, busiest first — "strongest first" reads as most
 * active (`count`, then most recently seen) until row-level SNR lands (GAP 2). */
export function foundInside(s: Selection, rows: readonly Row[]): Row[] {
  return rows
    .filter((r) => r.f_hi_hz >= s.f_lo && r.f_lo_hz <= s.f_hi)
    .sort((a, b) => b.count - a.count || b.last_seen_s - a.last_seen_s);
}

/** Listen-to-all targets for a selection's found-inside rows (dock/api.ts `startListen`, one call
 * per emitter; a budget refusal at `4503` is shown per dock entry, not here). */
export function listenAllTargets(rows: readonly Row[]): ListenTarget[] {
  return rows.map((r) => ({ kind: "emitter", emitterId: r.id, label: `${(r.f_center_hz / 1e6).toFixed(4)} MHz` }));
}

/** Starts a band-scoped recording of a selection's extent (§4.5 "Export clip"; GAP 1 interim: this
 * records forward from now, not from the always-on buffer). */
export async function recordSelectionClip(client: PostClient, s: Selection): Promise<{ ok: true; kinds: string[] } | { ok: false; message: string }> {
  try {
    const r = await client.post<{ recording: { kinds: string[] } }>("/api/outputs/record/start", {
      band: { f_lo: s.f_lo, f_hi: s.f_hi }, kinds: RECORD_KINDS,
    });
    return { ok: true, kinds: r.recording.kinds };
  } catch (e) {
    return { ok: false, message: apiErrorText(e) };
  }
}
