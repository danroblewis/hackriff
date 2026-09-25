// T-821 (MAP-21): the Research panel's state — collections, their markers and the annotations filed
// in them — and the ONE model both surfaces read: the canvas draws each collection as an overlay
// layer, the table lists every mark as a row, and both render from this slice, so an edit made in
// either (a rename, a delete, a visibility toggle, a selection) shows in the other on its next frame.
// Top-level key: `research`. docs/23 §9, docs/25 §3, docs/api.md "Marker collections" + "Annotations".
//
// THIN CLIENT: every figure here is the backend's own. A marker's `f_lo_hz`/`f_hi_hz` and
// `t_start_s`/`t_end_s` are computed by the server (docs/api.md: "a client never derives an extent
// from two fields"); this file only orders, filters and places them. The one thing it adds is a
// DRAWING floor (a pin or a point is widened to a few pixels so it can be seen and clicked), which is
// never read back as a bandwidth or a duration — `marks.ts`'s `minPx` rule.
//
// Nothing here reaches a device route: selection and layer visibility are view state; rename, delete
// and create are `PUT`/`DELETE`/`POST` on `/api/collections`, `/api/markers` and `/api/annotations`,
// which are authoring acts the server audits with no `device` key.
import type { AppState } from "../state";
import type { LayerId, PaneLayers } from "../../surface/layers";
import type { MarkBox } from "../../surface/marks";
import type { Box } from "../../surface/lattice";

// ---- wire shapes (docs/api.md), only the fields read ----
export interface Collection {
  id: string; name: string; note: string | null; color: string | null;
  visible: boolean; reserved: boolean; member_count: number;
}
export interface Marker {
  id: string; collection_id: string; name: string; note: string | null;
  f_center_hz: number; bandwidth_hz: number | null; f_lo_hz: number; f_hi_hz: number;
  t_center_s: number | null; duration_s: number | null; t_start_s: number | null; t_end_s: number | null;
  provenance?: { tier?: string | null; device_id?: string | null } | null;
}
export interface Annotation {
  id: string; collection_id: string | null; kind: "text" | "box" | "marker";
  f_lo_hz: number; f_hi_hz: number; t0_s: number; t1_s: number; label: string; body: string | null;
  provenance?: { tier?: string | null; device_id?: string | null } | null;
}

export interface ResearchSlice {
  open: boolean;
  loaded: boolean;
  collections: readonly Collection[];
  markers: readonly Marker[];
  annotations: readonly Annotation[];
  /** The selected mark's row key (`rowKey`), shared by the table's row highlight and the canvas's. */
  selected: string | null;
}
export interface ResearchState { research: ResearchSlice }

export const researchInitial = (): ResearchState => ({
  research: { open: false, loaded: false, collections: [], markers: [], annotations: [], selected: null },
});

/** The reserved `Bookmarks` collection's fixed id (docs/api.md). */
export const BOOKMARKS_ID = "00000000-0000-7000-8000-000000000b00";
/** Annotations filed in no collection draw under the `research` layer and group as "Unfiled". */
export const UNFILED = "Unfiled";

export const collectionLayer = (id: string): LayerId => `collection:${id}`;

// ---- actions: pure `(state) => patch` ----
export const setResearchOpen = (open: boolean) => (s: AppState): Partial<AppState> =>
  (s.research.open === open ? {} : { research: { ...s.research, open } });

export const setResearchData = (d: { collections: Collection[]; markers: Marker[]; annotations: Annotation[] }) =>
  (s: AppState): Partial<AppState> => {
    const keys = new Set([...d.markers.map((m) => rowKey("marker", m.id)), ...d.annotations.map((a) => rowKey("annotation", a.id))]);
    // A selection whose mark was deleted (here or by another client) goes with it: no ghost highlight.
    const selected = s.research.selected !== null && keys.has(s.research.selected) ? s.research.selected : null;
    return { research: { ...s.research, ...d, loaded: true, selected } };
  };

/** Select a mark (or clear with null). View state only: highlights the row and the box. */
export const selectResearch = (key: string | null) => (s: AppState): Partial<AppState> =>
  (s.research.selected === key ? {} : { research: { ...s.research, selected: key } });

/**
 * The collection's stored layer default, toggled from the panel. Every pane's own override for that
 * collection is dropped, so the panel's switch is what every pane shows until a pane's layers menu
 * diverges it again (docs/24 §13: the menu writes the active pane only).
 */
export const setCollectionVisible = (id: string, visible: boolean) => (s: AppState): Partial<AppState> => {
  const lid = collectionLayer(id);
  const layers: Record<string, PaneLayers> = {};
  let dropped = false;
  for (const [pid, reg] of Object.entries(s.layers)) {
    const keep = reg.layers.filter((l) => l.id !== lid);
    if (keep.length !== reg.layers.length) dropped = true;
    layers[pid] = keep.length === reg.layers.length ? reg : { ...reg, layers: keep };
  }
  return {
    research: { ...s.research, collections: s.research.collections.map((c) => (c.id === id ? { ...c, visible } : c)) },
    ...(dropped ? { layers } : {}),
  };
};

/** Whether a collection draws on a pane: the pane's own override if its layers menu set one, else
 * the collection's stored default. */
export function collectionVisibleOn(reg: PaneLayers, c: Collection): boolean {
  return reg.layers.find((l) => l.id === collectionLayer(c.id))?.visible ?? c.visible;
}

// ---- the rows: every mark is also a row ----
export type RowKind = "marker" | "annotation";
export const rowKey = (kind: RowKind, id: string): string => `${kind}:${id}`;

export interface ResearchRow {
  key: string; kind: RowKind; id: string;
  /** "pin" (frequency only), "point", "box", or the annotation's own kind. */
  shape: string;
  name: string; note: string | null;
  collectionId: string | null; collectionName: string;
  fLoHz: number; fHiHz: number; fCenterHz: number;
  /** Capture-clock seconds; null for a frequency-only pin. */
  tStartS: number | null; tEndS: number | null;
  tier: string | null;
}

export function researchRows(r: Pick<ResearchSlice, "collections" | "markers" | "annotations">): ResearchRow[] {
  const names = new Map(r.collections.map((c) => [c.id, c.name]));
  const rows: ResearchRow[] = [];
  for (const m of r.markers) {
    rows.push({
      key: rowKey("marker", m.id), kind: "marker", id: m.id,
      shape: m.t_center_s === null ? "pin" : m.duration_s === null ? "point" : "box",
      name: m.name, note: m.note, collectionId: m.collection_id, collectionName: names.get(m.collection_id) ?? "?",
      fLoHz: m.f_lo_hz, fHiHz: m.f_hi_hz, fCenterHz: m.f_center_hz,
      tStartS: m.t_start_s, tEndS: m.t_end_s, tier: m.provenance?.tier ?? null,
    });
  }
  for (const a of r.annotations) {
    rows.push({
      key: rowKey("annotation", a.id), kind: "annotation", id: a.id, shape: a.kind,
      name: a.label, note: a.body, collectionId: a.collection_id,
      collectionName: a.collection_id === null ? UNFILED : names.get(a.collection_id) ?? UNFILED,
      fLoHz: a.f_lo_hz, fHiHz: a.f_hi_hz, fCenterHz: (a.f_lo_hz + a.f_hi_hz) / 2,
      tStartS: a.t0_s, tEndS: a.t1_s, tier: a.provenance?.tier ?? null,
    });
  }
  return rows;
}

export type SortKey = "kind" | "name" | "collection" | "freq" | "time";
export interface RowFilter {
  kind: RowKind | "all";
  /** Case-insensitive substring over name, note and collection. */
  text: string;
  /** Only rows in this collection (null = every collection). */
  collectionId: string | null;
  /** Only rows overlapping this window: frequency always, time when given. A pin matches every time. */
  window: { loHz: number; hiHz: number; t0S?: number; t1S?: number } | null;
}

export function filterRows(rows: readonly ResearchRow[], f: RowFilter): ResearchRow[] {
  const q = f.text.trim().toLowerCase();
  return rows.filter((r) => {
    if (f.kind !== "all" && r.kind !== f.kind) return false;
    if (f.collectionId !== null && r.collectionId !== f.collectionId) return false;
    if (q && ![r.name, r.note ?? "", r.collectionName].some((s) => s.toLowerCase().includes(q))) return false;
    const w = f.window;
    if (w) {
      if (r.fHiHz < w.loHz || r.fLoHz > w.hiHz) return false;
      if (w.t0S !== undefined && w.t1S !== undefined && r.tStartS !== null && r.tEndS !== null
        && (r.tEndS < w.t0S || r.tStartS > w.t1S)) return false;
    }
    return true;
  });
}

/** Stable sort by one column; ties keep the backend's creation order. Pins sort as oldest in time. */
export function sortRows(rows: readonly ResearchRow[], key: SortKey, dir: 1 | -1): ResearchRow[] {
  const val = (r: ResearchRow): string | number => {
    switch (key) {
      case "kind": return `${r.kind}:${r.shape}`;
      case "name": return r.name.toLowerCase();
      case "collection": return r.collectionName.toLowerCase();
      case "freq": return r.fCenterHz;
      case "time": return r.tStartS ?? -Infinity;
    }
  };
  return rows.map((r, i) => ({ r, i })).sort((a, b) => {
    const x = val(a.r), y = val(b.r);
    const c = x < y ? -1 : x > y ? 1 : 0;
    return c !== 0 ? c * dir : a.i - b.i;
  }).map((x) => x.r);
}

// ---- the canvas: each collection is an overlay layer ----

/** Default ink for a collection with no colour of its own (the mockup's "mine" cream). */
export const MINE_MARK: readonly [number, number, number, number] = [0.953, 0.890, 0.749, 0.9];
/** The selected mark: the selection amber, full alpha, heavier stroke. */
export const RESEARCH_SELECTED_MARK: readonly [number, number, number, number] = [0.941, 0.647, 0.259, 1];
/** A pin or a point is drawn (and hit) at least this many CSS-ish device px on each axis. */
export const RESEARCH_MIN_PX = 8;

export function parseColor(c: string | null): readonly [number, number, number, number] {
  const m = c ? /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(c) : null;
  return m ? [parseInt(m[1], 16) / 255, parseInt(m[2], 16) / 255, parseInt(m[3], 16) / 255, 0.9] : MINE_MARK;
}

/**
 * The rectangles for the rows whose layer (`layerOf(row)`) is visible on this pane, placed in the
 * pane's own box. A pin spans the pane's whole time extent (it marks a band for all time); a point
 * or a zero-width marker is widened about its centre to `RESEARCH_MIN_PX` so it can be seen and
 * clicked — drawing only, never a claimed extent.
 */
export function researchMarkBoxes(
  rows: readonly ResearchRow[], colors: ReadonlyMap<string, readonly [number, number, number, number]>,
  visible: (row: ResearchRow) => boolean, selected: string | null,
  paneBox: Box, rectPx: { w: number; h: number },
): MarkBox[] {
  const hzPerPx = (paneBox.f1Hz - paneBox.f0Hz) / Math.max(1, rectPx.w);
  const nsPerPx = (paneBox.t1Ns - paneBox.t0Ns) / Math.max(1, rectPx.h);
  const minF = RESEARCH_MIN_PX * hzPerPx, minT = RESEARCH_MIN_PX * nsPerPx;
  const out: MarkBox[] = [];
  for (const r of rows) {
    if (!visible(r)) continue;
    let f0 = r.fLoHz, f1 = r.fHiHz;
    if (f1 - f0 < minF) { const c = (f0 + f1) / 2; f0 = c - minF / 2; f1 = c + minF / 2; }
    let t0: number, t1: number;
    if (r.tStartS === null || r.tEndS === null) { t0 = paneBox.t0Ns; t1 = paneBox.t1Ns; } else {
      t0 = r.tStartS * 1e9; t1 = r.tEndS * 1e9;
      if (t1 - t0 < minT) { const c = (t0 + t1) / 2; t0 = c - minT / 2; t1 = c + minT / 2; }
    }
    const sel = r.key === selected;
    out.push({
      id: r.key, kind: "research-box", f0Hz: f0, f1Hz: f1, t0Ns: t0, t1Ns: t1,
      rgba: sel ? RESEARCH_SELECTED_MARK : colors.get(r.collectionId ?? "") ?? MINE_MARK,
      open: false, strokePx: sel ? 3 : 1,
    });
  }
  return out;
}
