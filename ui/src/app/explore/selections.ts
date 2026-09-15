// Selections model for Explore (ADR-0013 §4.2, §4.5, T-151): one shared `SelectionStore` (the old
// page's offline-sync model, T-044/T-052) mirrored into the store's `selections` slice, plus the
// pure "found inside" and Listen-to-all helpers the focus panel uses for a selection.
import { apiBackend, type ApiClient, type Selection, SelectionStore, sortSelections, syncText } from "../../selections";
import { RECORD_KINDS } from "../../outputs";
import { apiErrorText } from "./format";
import type { AppContext } from "../context";
import type { ListenTarget } from "../dock/api";
import type { Row } from "./inventory";
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
  setInterval(() => void store.flush(), 15_000);
  return store;
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
