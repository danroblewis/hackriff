// Explore sidebar and focus state (ADR-0013 §3.1). Owner: T-151. Top-level keys: focus, inventory,
// selections.
import type { Selection } from "../../selections";
import type { AppState } from "../state";
// `./inventory` (the richer `/api/inventory` row shape) imports `InventoryTab`/`InventorySortKey`
// from this file as types only, so this reverse import is also type-only: nothing here runs before
// the other module is initialised.
import type { Row as InventoryRow } from "./inventory";
import { sameSpecWindow, type PaneWindowSpec } from "./pane-window";

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

/** One pane's own answer: the window it asked about and the rows it got back (T-1002). */
export interface PaneInventory {
  spec: PaneWindowSpec;
  /** Rows of THIS pane's (time × frequency) window, by id. */
  rows: Readonly<Record<string, InventoryRow>>;
  /** The window those rows are of, or `null` while it is unknown (see [[InventoryWindow]]). */
  window: InventoryWindow | null;
  loadedAtS: number | null;
}

/** The pane the lists are showing, as the chrome names it (`centre/active-pane.ts`): its id, its
 * 1-based position in layout order, and how many panes there are. `label` is what a heading says
 * — and only worth saying when `count > 1`, because a badge on the only viewport there is carries
 * no information (docs/23 §10.6 P1). */
export interface ActiveInventoryPane { id: string; n: number; count: number; label: string }

export interface InventorySlice {
  tab: InventoryTab; sort: { key: InventorySortKey; dir: 1 | -1 };
  /** The ACTIVE pane's rows, by id — the mirror the lists, the focus panel and the menus read.
   * Every pane's own rows are in [[panes]]; this is the one the chrome is scoped to. */
  rows: Readonly<Record<string, InventoryRow>>;
  /** The window [[rows]] are of, or `null` while it is unknown (see [[InventoryWindow]]). */
  window: InventoryWindow | null;
  /**
   * Each pane's own inventory, by pane id (T-1002) — the answer to that pane's `(t, f)` window, so
   * a pane frozen on a past signal and a pane at the live edge draw their own boxes. The empty key
   * `""` is the **unpaned** window: before the surface publishes a pane registry (and in a test
   * that seeds rows directly), there is one window and it lives there.
   */
  panes: Readonly<Record<string, PaneInventory>>;
  /** Which pane [[rows]] mirrors, or `null` when no registry has been published yet. */
  active: ActiveInventoryPane | null;
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
  inventory: { tab: "confirmed", sort: { key: "freq", dir: 1 }, rows: {}, window: null, panes: {}, active: null, loadedAtS: null, error: null },
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

// ---- one window per pane, and the mirror the chrome reads (T-1002) -------------------------
//
// Every write below lands in a PANE's entry and the active pane's entry is then mirrored into
// `rows`/`window`. That is the whole shape of the split-view fix: a pane's boxes read its own
// entry, so touching pane 1 cannot re-scope pane 2; the lists, the focus panel and the menus read
// the mirror, so they follow the pane the user is acting on and can say which it is.
//
// The **unpaned** key `""` is not a special case in the readers, only in the writers: with no pane
// registry published, `active` is `null`, the mirror key is `""`, and the single-window behaviour
// every caller had before this ticket is exactly what a one-entry registry does.

/** Which pane entry [[rows]] mirrors. */
const mirrorKey = (inv: InventorySlice): string => inv.active?.id ?? "";

/** Re-derives the mirror after any write to [[InventorySlice.panes]]. The mirror is a *projection*,
 * never a second copy that can drift: a pane with no answer yet mirrors as no rows and an unknown
 * window, which is what it is. */
function mirror(inv: InventorySlice): InventorySlice {
  const p = inv.panes[mirrorKey(inv)];
  const rows = p?.rows ?? {};
  const window = p?.window ?? null;
  const loadedAtS = p?.loadedAtS ?? null;
  return rows === inv.rows && window === inv.window && loadedAtS === inv.loadedAtS
    ? inv
    : { ...inv, rows, window, loadedAtS };
}

/** Applies `edit` to every pane's rows (a row shown in two panes is the same row) and re-mirrors. */
function editEveryPane(
  inv: InventorySlice, edit: (rows: Readonly<Record<string, InventoryRow>>) => Readonly<Record<string, InventoryRow>> | null,
): InventorySlice {
  let changed = false;
  const panes: Record<string, PaneInventory> = {};
  for (const [id, p] of Object.entries(inv.panes)) {
    const rows = edit(p.rows);
    if (rows === null || rows === p.rows) { panes[id] = p; continue; }
    panes[id] = { ...p, rows };
    changed = true;
  }
  return changed ? mirror({ ...inv, panes }) : inv;
}

/** The unpaned entry, for the writers that name no pane. */
const unpaned = (inv: InventorySlice): PaneInventory => inv.panes[""] ?? {
  spec: { id: "", n: 0, loHz: null, hiHz: null, live: true, tS: null, spanS: 0 },
  rows: {}, window: null, loadedAtS: null,
};

/** Replaces the loaded rows of the unpaned window (a fresh poll or post-action reload, and the
 * shape a test seeds state in); clears any poll error. */
export const setInventoryRows = (rows: Readonly<Record<string, InventoryRow>>, loadedAtS: number) => (s: AppState): Partial<AppState> => ({
  inventory: mirror({
    ...s.inventory, error: null,
    panes: { ...s.inventory.panes, "": { ...unpaned(s.inventory), rows, loadedAtS } },
  }),
});

export const setInventoryError = (error: string) => (s: AppState): Partial<AppState> => ({ inventory: { ...s.inventory, error } });

/** Records the window the unpaned list was asked about, or `null` for *no window known* (T-379).
 * Setting `null` also clears the rows: rows are only ever rows *of a window*, so keeping the
 * previous window's rows on screen beside a "window unknown" note would be the stale-list bug
 * T-263 fixed. */
export const setInventoryWindow = (window: InventoryWindow | null) => (s: AppState): Partial<AppState> => ({
  inventory: mirror({
    ...s.inventory,
    panes: {
      ...s.inventory.panes,
      "": window === null ? { ...unpaned(s.inventory), window: null, rows: {} } : { ...unpaned(s.inventory), window },
    },
  }),
});

/**
 * Publishes the panes on screen and which of them is active (T-1002) — the surface's one write of
 * the view's shape into the inventory, on every frame the set of pane windows changes.
 *
 * A pane whose window is unchanged **keeps** its rows: a republish caused by another pane moving,
 * or by the active pane changing, must not blank a pane that is still asking the same question.
 * A pane whose window MOVED keeps them too, for the one fetch it takes to answer — the same
 * staleness a scrub has always had, bounded by the reload this publish triggers — while a pane
 * that has just appeared has none, because nothing has been asked about its window yet.
 *
 * The **first** publish hands the unpaned entry to the active pane rather than dropping it: that
 * entry IS the window the active pane was mirroring a moment ago, so handing it over renames it
 * instead of blanking the lists for a round trip.
 */
export const setInventoryPanes = (specs: readonly PaneWindowSpec[], active: ActiveInventoryPane | null) => (s: AppState): Partial<AppState> => {
  const inv = s.inventory;
  const firstPublish = inv.active === null && inv.panes[""] !== undefined;
  const panes: Record<string, PaneInventory> = {};
  for (const spec of specs) {
    const prev = inv.panes[spec.id] ?? (firstPublish && spec.id === active?.id ? inv.panes[""] : undefined);
    if (!prev) { panes[spec.id] = { spec, rows: {}, window: null, loadedAtS: null }; continue; }
    // Unchanged window AND unchanged name: the same entry object, so a republish that moved nothing
    // is not a store change at all (this runs on the frame hook).
    panes[spec.id] = sameSpecWindow(prev.spec, spec) && prev.spec.id === spec.id && prev.spec.n === spec.n
      ? prev
      : { ...prev, spec };
  }
  const sameActive = inv.active?.id === active?.id && inv.active?.n === active?.n && inv.active?.count === active?.count;
  const sameSet = Object.keys(panes).length === Object.keys(inv.panes).length
    && Object.entries(panes).every(([id, p]) => inv.panes[id] === p);
  if (sameActive && sameSet) return {};
  return { inventory: mirror({ ...inv, panes, active }) };
};

/** One pane's answer: the rows of its window, or `null` for *this pane's window is not known yet*
 * (T-379 — said, never invented). A pane that has closed since the request was made is dropped on
 * the floor rather than resurrected. */
export const setPaneInventory = (
  paneId: string, got: { rows: Readonly<Record<string, InventoryRow>>; window: InventoryWindow } | null, loadedAtS: number,
) => (s: AppState): Partial<AppState> => {
  const prev = s.inventory.panes[paneId];
  if (!prev) return {};
  const next: PaneInventory = got === null
    ? { ...prev, rows: {}, window: null, loadedAtS }
    : { ...prev, rows: got.rows, window: got.window, loadedAtS };
  return { inventory: mirror({ ...s.inventory, error: null, panes: { ...s.inventory.panes, [paneId]: next } }) };
};

/** T-187: optimistic delete — drops one row from every pane showing it, before the server has
 * confirmed the `DELETE`. A no-op if the row is already gone (e.g. a reload raced it out). */
export const removeInventoryRowLocal = (id: string) => (s: AppState): Partial<AppState> => {
  const inv = editEveryPane(s.inventory, (rows) => {
    if (!(id in rows)) return null;
    const next = { ...rows };
    delete next[id];
    return next;
  });
  return inv === s.inventory ? {} : { inventory: inv };
};

/** T-187: reverts [[removeInventoryRowLocal]] when the `DELETE` the caller optimistically applied
 * is refused — puts the row back exactly as it was, so a network blip or a 4xx never silently
 * drops a candidate the user did not actually delete. It goes back into the ACTIVE pane's entry:
 * that is the list the user pressed Delete in. */
export const restoreInventoryRowLocal = (row: InventoryRow) => (s: AppState): Partial<AppState> => {
  const inv = s.inventory;
  const key = mirrorKey(inv);
  const prev = inv.panes[key] ?? unpaned(inv);
  return { inventory: mirror({ ...inv, panes: { ...inv.panes, [key]: { ...prev, rows: { ...prev.rows, [row.id]: row } } } }) };
};

/** Patches one already-loaded row in place (T-193: an optimistic user-band commit/reset lands
 * without waiting for the next poll; T-388's presence push extends a box's top), in every pane
 * that has it and leaving every other row untouched; a no-op if the row isn't loaded anywhere
 * (e.g. it scrolled out of every view span in the meantime). */
export const patchInventoryRow = (id: string, patch: Partial<InventoryRow>) => (s: AppState): Partial<AppState> => {
  const inv = editEveryPane(s.inventory, (rows) => (rows[id] ? { ...rows, [id]: { ...rows[id], ...patch } } : null));
  return inv === s.inventory ? {} : { inventory: inv };
};

const EMPTY_ROWS: Readonly<Record<string, InventoryRow>> = Object.freeze({});

/**
 * The rows a pane draws: **its own**, and nothing else (T-1002).
 *
 * With no pane registry published (a test that seeded rows, the moment before the surface's first
 * frame) there is one window and every pane draws it — the behaviour before this ticket. With a
 * registry, a pane that is not in it, or has not been answered yet, draws NOTHING rather than
 * borrowing the active pane's rows: a box is a claim about this pane's window, and drawing another
 * window's boxes here is exactly the defect the split view exposed.
 */
export function paneRows(inv: InventorySlice, paneId: string): Readonly<Record<string, InventoryRow>> {
  const p = inv.panes[paneId];
  if (p) return p.rows;
  return inv.active === null ? inv.rows : EMPTY_ROWS;
}

/** Mirrors `SelectionStore`'s list into the store (§3.2); `sync` is a short status string
 * (`selections.ts` `syncText`). */
export const setSelections = (list: readonly Selection[], sync: string): Partial<AppState> => ({ selections: { list, sync } });
