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
/**
 * Whether the **detail card** — the bottom sheet (T-803) that shows what is selected, the lists a
 * pill opens and the Explore drawer — is on screen at all (T-1026, user 2026-09-25).
 *
 * The card is a Google-Maps place card: **hidden until the viewer clicks something.** It opens when a
 * feature is selected ([[focusSignal]], [[focusSelection]] — a box, a pin, a marker, a list row) or
 * when an inventory pill asks for a list ([[openCardOnList]]); it closes on a click on bare map, on
 * its ×, and on Escape ([[closeCard]]). Openness is view state and nothing else: it reaches no route,
 * and — unlike the sheet's peek/half/full *size*, which is a per-viewer `localStorage` preference —
 * it is never persisted, because it belongs to the current selection and not to the browser profile.
 */
export interface CardSlice { open: boolean }

export interface ExploreState { card: CardSlice; focus: Focus; bandEdit: string | null; inventory: InventorySlice; selections: SelectionsSlice }

export const exploreInitial = (): ExploreState => ({
  // T-1026: hidden. The first paint of the map has no card on it.
  card: { open: false },
  focus: { kind: "none" },
  bandEdit: null,
  inventory: { tab: "confirmed", sort: { key: "freq", dir: 1 }, rows: {}, window: null, loadedAtS: null, error: null },
  selections: { list: [], sync: "" },
});

/** Focus a signal; switches the inventory tab to the row's state so the row is visible, and opens
 * the detail card on it (T-1026: selecting a feature is what puts the card on screen). */
export const focusSignal = (id: string) => (s: AppState): Partial<AppState> => {
  const row = s.inventory.rows[id];
  const tab: InventoryTab | null = row && (row.state === "confirmed" || row.state === "candidate") ? row.state : null;
  return tab && tab !== s.inventory.tab
    ? { card: { open: true }, focus: { kind: "signal", id }, inventory: { ...s.inventory, tab } }
    : { card: { open: true }, focus: { kind: "signal", id } };
};

export const focusSelection = (id: string) => (): Partial<AppState> => ({ card: { open: true }, focus: { kind: "selection", id } });

/**
 * Put the card on screen showing one of the inventory lists — what an inventory pill does (T-997's
 * pills, T-1026's card): the list is chosen here, so a pill needs one write and the card can never
 * open on a list other than the one the pill named. No selection is invented: the card opens on the
 * list, with whatever (or nothing) was focused still focused.
 */
export const openCardOnList = (tab: InventoryTab) => (s: AppState): Partial<AppState> => ({
  card: { open: true }, inventory: s.inventory.tab === tab ? s.inventory : { ...s.inventory, tab },
});

/**
 * Close the card and clear the selection — a click on bare map, the ×, or Escape (T-1026).
 *
 * The selection goes with it deliberately: a card closed over a still-selected box would leave the
 * map with a highlighted feature and no card, and clicking that same box again would then change
 * nothing to re-open it. "Click the back of the map and it goes away" means the selection went away.
 */
export const closeCard = () => (s: AppState): Partial<AppState> =>
  (s.card.open === false && s.focus.kind === "none" ? {} : { card: { open: false }, focus: { kind: "none" } });

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
