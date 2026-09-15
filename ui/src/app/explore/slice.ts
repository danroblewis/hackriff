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
export interface InventorySlice {
  tab: InventoryTab; sort: { key: InventorySortKey; dir: 1 | -1 };
  /** Rows of the current view's span, by id (the `/api/inventory` row shape, unmodified). */
  rows: Readonly<Record<string, InventoryRow>>;
  loadedAtS: number | null; error: string | null;
}

export interface SelectionsSlice { list: readonly Selection[]; sync: string }

export interface ExploreState { focus: Focus; inventory: InventorySlice; selections: SelectionsSlice }

export const exploreInitial = (): ExploreState => ({
  focus: { kind: "none" },
  inventory: { tab: "confirmed", sort: { key: "freq", dir: 1 }, rows: {}, loadedAtS: null, error: null },
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

export const setInventoryTab = (tab: InventoryTab) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, tab } });

export const setInventorySort = (sort: InventorySlice["sort"]) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, sort } });

/** Replaces the loaded rows (a fresh poll or post-action reload); clears any poll error. */
export const setInventoryRows = (rows: Readonly<Record<string, InventoryRow>>, loadedAtS: number) => (s: AppState): Partial<AppState> => ({
  inventory: { ...s.inventory, rows, loadedAtS, error: null },
});

export const setInventoryError = (error: string) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, error } });

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
