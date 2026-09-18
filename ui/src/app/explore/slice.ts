// Explore sidebar and focus state (ADR-0013 §3.1). Owner: T-151. Top-level keys: focus, inventory,
// selections.
import type { Selection } from "../../selections";
import type { AppState } from "../state";
// `./inventory` (the richer `/api/inventory` row shape) imports `InventoryTab`/`InventorySortKey`
// from this file as types only, so this reverse import is also type-only: nothing here runs before
// the other module is initialised.
import type { Row as InventoryRow } from "./inventory";

/** What the right focus panel shows (Explore). */
export type Focus = { kind: "none" } | { kind: "signal"; id: string } | { kind: "selection"; id: string };

export type InventoryTab = "confirmed" | "candidate";
export type InventorySortKey = "freq" | "last_seen" | "count" | "bandwidth";

/** Whether the front end ever sampled the listed window, as `GET /api/coverage` served it (T-368);
 * `null` is **not known**, which is neither of the other two and must never be rendered as either. */
export type WindowCoverage = "observed" | "unobserved" | null;

/**
 * The window the lists were last asked about, and what the backend says was sampled in it (T-379).
 *
 * This exists so an empty sidebar can say *which* emptiness it is. Three states, kept apart:
 * `null` — no window was asked about (no live edge reported yet); `coverage: "unobserved"` — the
 * window was asked about and nothing ever looked there; `coverage: "observed"` — the receiver was
 * listening and heard nothing. Only the last is a finding about the air. Collapsing them is the
 * failure the whole-UI window rule names: "we have it but didn't render it" and "there was nothing
 * to render" look identical on screen and are opposite bugs.
 */
export interface InventoryWindow { t0: number; t1: number; coverage: WindowCoverage }

export interface InventorySlice {
  tab: InventoryTab; sort: { key: InventorySortKey; dir: 1 | -1 };
  /** Rows of the current view's span, by id (the `/api/inventory` row shape, unmodified). */
  rows: Readonly<Record<string, InventoryRow>>;
  /** The window those rows are of, or `null` while it is unknown (see [[InventoryWindow]]). */
  window: InventoryWindow | null;
  loadedAtS: number | null; error: string | null;
}

export interface SelectionsSlice { list: readonly Selection[]; sync: string }

/**
 * The Confirmed signal whose band the next region stroke sets, or `null` for none (T-458).
 *
 * T-193's user-band override was left with **no setter at all** when T-456 gave the drag to panning
 * — stored state a reader still honoured (`surface/marks.ts` draws the override in place of the
 * measured extent) and nothing could cause. The override is kept, and this is its input: "Adjust
 * band" on a Confirmed row arms it, the next shift+drag on the surface is that row's new band
 * instead of a new selection, and the arming clears either way.
 *
 * It is a **named target**, not a guess from what is focused or from where the stroke began: a
 * band override rewrites what the user is shown about a specific emitter, so which emitter must be
 * something the user said rather than something the gesture inferred.
 */
export interface ExploreState { focus: Focus; bandEdit: string | null; inventory: InventorySlice; selections: SelectionsSlice }

export const exploreInitial = (): ExploreState => ({
  focus: { kind: "none" },
  bandEdit: null,
  inventory: { tab: "confirmed", sort: { key: "freq", dir: 1 }, rows: {}, window: null, loadedAtS: null, error: null },
  selections: { list: [], sync: "" },
});

/** Focus a signal; switches the inventory tab to the row's state so the row is visible. */
export const focusSignal = (id: string) => (s: AppState): Partial<AppState> => {
  const row = s.inventory.rows[id];
  const tab: InventoryTab | null = row && (row.state === "confirmed" || row.state === "candidate") ? row.state : null;
  return tab && tab !== s.inventory.tab
    ? { focus: { kind: "signal", id }, inventory: { ...s.inventory, tab } }
    : { focus: { kind: "signal", id } };
};

export const focusSelection = (id: string) => (): Partial<AppState> => ({ focus: { kind: "selection", id } });

/** Arms (or disarms, with `null`) the next region stroke as a band override for that row — see
 * [[ExploreState.bandEdit]]. */
export const setBandEdit = (id: string | null) => (): Partial<AppState> => ({ bandEdit: id });

export const setInventoryTab = (tab: InventoryTab) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, tab } });

export const setInventorySort = (sort: InventorySlice["sort"]) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, sort } });

/** Replaces the loaded rows (a fresh poll or post-action reload); clears any poll error. */
export const setInventoryRows = (rows: Readonly<Record<string, InventoryRow>>, loadedAtS: number) => (s: AppState): Partial<AppState> => ({
  inventory: { ...s.inventory, rows, loadedAtS, error: null },
});

export const setInventoryError = (error: string) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, error } });

/** Records the window the lists were asked about, or `null` for *no window known* (T-379). Setting
 * `null` also clears the rows: rows are only ever rows *of a window*, so keeping the previous
 * window's rows on screen beside a "window unknown" note would be the stale-list bug T-263 fixed. */
export const setInventoryWindow = (window: InventoryWindow | null) => (s: AppState): Partial<AppState> => ({
  inventory: window === null
    ? { ...s.inventory, window: null, rows: {} }
    : { ...s.inventory, window },
});

/** T-187: optimistic delete — drops one row from the loaded set immediately, before the server has
 * confirmed the `DELETE`. A no-op if the row is already gone (e.g. a reload raced it out). */
export const removeInventoryRowLocal = (id: string) => (s: AppState): Partial<AppState> => {
  if (!(id in s.inventory.rows)) return {};
  const rows = { ...s.inventory.rows };
  delete rows[id];
  return { inventory: { ...s.inventory, rows } };
};

/** T-187: reverts [[removeInventoryRowLocal]] when the `DELETE` the caller optimistically applied
 * is refused — puts the row back exactly as it was, so a network blip or a 4xx never silently
 * drops a candidate the user did not actually delete. */
export const restoreInventoryRowLocal = (row: InventoryRow) => (s: AppState): Partial<AppState> => ({
  inventory: { ...s.inventory, rows: { ...s.inventory.rows, [row.id]: row } },
});

/** Patches one already-loaded row in place (T-193: an optimistic user-band commit/reset lands
 * without waiting for the next poll), leaving every other row untouched; a no-op if the row isn't
 * loaded (e.g. it scrolled out of the view span in the meantime). */
export const patchInventoryRow = (id: string, patch: Partial<InventoryRow>) => (s: AppState): Partial<AppState> => {
  const row = s.inventory.rows[id];
  return row ? { inventory: { ...s.inventory, rows: { ...s.inventory.rows, [id]: { ...row, ...patch } } } } : {};
};


/** Mirrors `SelectionStore`'s list into the store (§3.2); `sync` is a short status string
 * (`selections.ts` `syncText`). */
export const setSelections = (list: readonly Selection[], sync: string): Partial<AppState> => ({ selections: { list, sync } });
