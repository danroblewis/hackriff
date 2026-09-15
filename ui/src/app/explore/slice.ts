// Explore sidebar and focus state (ADR-0013 §3.1). Owner: T-151. Top-level keys: focus, inventory,
// selections.
import type { Row as InventoryRow } from "../../inventory";
import type { Selection } from "../../selections";
import type { AppState } from "../state";

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
