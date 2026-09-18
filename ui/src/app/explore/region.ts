// **Where a region stroke goes** (T-458).
//
// The surface's shift+drag (`surface/input.ts`) hands the mount a rectangle in (Hz, capture-ns).
// That rectangle has two possible destinations, and this file is the whole of the decision:
//
//   - `explore.bandEdit` armed by the context menu's "Adjust band" → **that Confirmed row's user
//     band** (`PUT /api/inventory/{id}/band`, T-191/T-193);
//   - otherwise → **a new selection** (`POST /api/selections`).
//
// It lives here rather than inside the centre mount because it is the part worth testing: a branch
// buried in a DOM mount is a branch only a browser can reach, and the failure it guards against is
// silent in both directions — a stroke that quietly rewrites a signal's band when the user meant a
// selection, or one that quietly makes a selection when they had just asked to adjust a band.
//
// **No signal logic.** Which rows exist and what state each is in are backend answers; whether a
// band is acceptable is the *server's* answer, reported in its own words. This file chooses a
// destination and nothing else.
import type { AppContext } from "../context";
import type { MarkRegion } from "../../surface/marks";
import { setUserBand, type BandClient, type Row } from "./inventory";
import { selectionStoreFor } from "./selections";
import { focusSelection, patchInventoryRow, setBandEdit } from "./slice";
import { toast } from "../state";

const S_TO_NS = 1e9;

/** Where a stroke is bound, decided from the armed target and the rows currently loaded. */
export type RegionDestination =
  | { kind: "selection" }
  | { kind: "band"; id: string }
  /** Armed for a row that is no longer in the viewed window — neither destination is right, and
   * falling back to "selection" would silently do something else with the user's stroke. */
  | { kind: "stale"; id: string };

/**
 * Which destination a stroke has.
 *
 * Pure, and deliberately the *only* place the question is asked. The `stale` arm exists because the
 * inventory is scoped to the viewed window (CLAUDE.md): a user can arm "Adjust band", scrub away,
 * and stroke — and the honest answer there is neither "set the band of a row we can no longer see"
 * nor "quietly make a selection instead".
 */
export function regionDestination(
  bandEdit: string | null, rows: Readonly<Record<string, Row>>,
): RegionDestination {
  if (!bandEdit) return { kind: "selection" };
  return bandEdit in rows ? { kind: "band", id: bandEdit } : { kind: "stale", id: bandEdit };
}

/**
 * Is this rectangle something that can be committed at all?
 *
 * `input.ts` already refuses a tap and a degenerate stroke, but the mount clamps the release corner
 * to the pane afterwards, and clamping can flatten a stroke that began on the very edge. Asked
 * again here so neither a zero-width selection nor a zero-width band override can be written.
 */
export const regionIsReal = (r: MarkRegion): boolean => r.f1Hz > r.f0Hz && r.t1Ns > r.t0Ns;

/**
 * Commit `region` to wherever [[regionDestination]] says, and disarm.
 *
 * Disarming is unconditional and happens *before* the request: an armed override that survived a
 * failed commit would fire on the user's next unrelated stroke, which is the same class of problem
 * as the setter-less override this ticket exists to fix — state doing something the user did not
 * ask for at a moment they were not thinking about it.
 */
export function commitRegion(ctx: AppContext, region: MarkRegion, fmtHz: (hz: number) => string): void {
  if (!regionIsReal(region)) return;
  const { store } = ctx;
  const fLo = region.f0Hz, fHi = region.f1Hz;
  const dest = regionDestination(store.get().bandEdit, store.get().inventory.rows);
  if (dest.kind !== "selection") store.set(setBandEdit(null));

  if (dest.kind === "stale") {
    store.set(toast(`Adjust band: ${dest.id.slice(0, 8)} is no longer in this window.`));
    return;
  }
  if (dest.kind === "band") {
    void setUserBand(ctx.client as BandClient, dest.id, fLo, fHi).then((res) => {
      // The server decides whether a band is acceptable (too far from the measured extent, over the
      // max width) and says why; a client-invented reason would be a second opinion about a rule
      // only the server holds.
      if (res.ok) {
        store.set(patchInventoryRow(dest.id, { user_band: res.entry.user_band }));
        store.set(toast(`Band set: ${fmtHz(fLo)} – ${fmtHz(fHi)}`));
      } else store.set(toast(`Adjust band: ${res.message}`));
    });
    return;
  }
  try {
    const sel = selectionStoreFor(ctx).add({
      f_lo: fLo, f_hi: fHi, t_lo: region.t0Ns / S_TO_NS, t_hi: region.t1Ns / S_TO_NS,
    });
    store.set(focusSelection(sel.id));
    store.set(toast(`Region: ${fmtHz(fLo)} – ${fmtHz(fHi)}`));
  } catch (e) {
    store.set(toast(`Region: ${e instanceof Error ? e.message : String(e)}`));
  }
}
