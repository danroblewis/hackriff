// MUI application state (ADR-0013 §3). Every slice is plain data from docs/api.md (or UI-only
// interaction state); no slice holds a derived signal measurement computed in the browser.
// Actions are pure `(state) => patch` functions so they are unit-testable without a DOM.
import type { Row as InventoryRow } from "../inventory";
import type { Selection } from "../selections";

export type Mode = "explore" | "decode";
export type Theme = "system" | "dark" | "light";

/** What the right focus panel shows (Explore). */
export type Focus = { kind: "none" } | { kind: "signal"; id: string } | { kind: "selection"; id: string };

/** The capture-timeline cursor: following live data, or reviewing a past instant (Unix s). */
export type TimeCursor = { live: true } | { live: false; tS: number };

export type ApiConn = "connecting" | "ok" | "offline" | "unauthorized";
export type StreamConn = "idle" | "connecting" | "live" | "reconnecting" | "unavailable";

export interface ConnSlice { api: ApiConn; spectrum: StreamConn; message: string }

/** `GET /api/control/state`, reduced to what the shell shows (T-150/T-155 extend it). */
export interface DeviceSlice {
  loaded: boolean; live: boolean; finished: boolean; contentClass: string | null;
  centerHz: number | null; sampleRateHz: number | null; rowsPerS: number | null; recording: boolean;
}

/** The live spectrum stream's geometry (header) and the client-side zoom (T-152 owns zoom). */
export interface LiveSlice {
  streamId: string | null; centerHz: number | null; bandwidthHz: number | null; bins: number | null;
  rowRateHz: number | null; view: { loHz: number; hiHz: number } | null;
}

export type InventoryTab = "confirmed" | "candidate";
export type InventorySortKey = "freq" | "last_seen" | "count" | "bandwidth";
export interface InventorySlice {
  tab: InventoryTab; sort: { key: InventorySortKey; dir: 1 | -1 };
  /** Rows of the current view's span, by id (the `/api/inventory` row shape, unmodified). */
  rows: Readonly<Record<string, InventoryRow>>;
  loadedAtS: number | null; error: string | null;
}

export interface SelectionsSlice { list: readonly Selection[]; sync: string }

/** One entry of the Outputs dock: a stream this page opened (audio) or a pipeline output. */
export interface OutputEntry {
  id: string; kind: "audio" | "records"; label: string; sub: string;
  state: "opening" | "live" | "refused" | "ended";
  /** TCP handshake target for "Copy address" (`/api/streams` tcp.addr + this). */
  tcpTarget: string | null;
  muted: boolean; levelDbfs: number | null; recordsPerS: number | null;
  emitterId: string | null; pipelineId: string | null; message: string | null;
}

export interface DecodeSlice {
  pipelineId: string | null; nodeId: string | null; frameSeq: number | null; fieldNodeId: number | null;
}

/** One-shot navigation requests from the top bar (Go to), consumed by T-151/T-152. */
export interface NavSlice { gotoHz: number | null; seq: number }

/** The Review drawer (T-155): alarms, survey report, region history, scheduler, device, bookmarks. */
export type ReviewTab = "alarms" | "report" | "history" | "scheduler" | "device" | "bookmarks";
export interface ReviewSlice {
  open: boolean; tab: ReviewTab;
  /** Region the drawer was opened on (e.g. a selection's History action); null = the live view. */
  region: { loHz: number; hiHz: number; t0?: number; t1?: number } | null;
}

export interface AppState {
  mode: Mode; theme: Theme; review: ReviewSlice;
  conn: ConnSlice; device: DeviceSlice; live: LiveSlice;
  focus: Focus; inventory: InventorySlice; selections: SelectionsSlice;
  outputs: readonly OutputEntry[]; time: TimeCursor; decode: DecodeSlice; nav: NavSlice;
  toast: { text: string; seq: number };
}

/** Per-viewer preferences kept in localStorage (never state that must persist). */
export interface Prefs { mode: Mode; theme: Theme }

export function parsePrefs(raw: string | null): Prefs {
  const d: Prefs = { mode: "explore", theme: "system" };
  if (!raw) return d;
  try {
    const p = JSON.parse(raw) as Partial<Prefs>;
    return {
      mode: p.mode === "decode" ? "decode" : "explore",
      theme: p.theme === "dark" || p.theme === "light" ? p.theme : "system",
    };
  } catch {
    return d;
  }
}

export function initialState(prefs: Prefs = parsePrefs(null)): AppState {
  return {
    mode: prefs.mode, theme: prefs.theme, review: { open: false, tab: "alarms", region: null },
    conn: { api: "connecting", spectrum: "idle", message: "" },
    device: { loaded: false, live: false, finished: false, contentClass: null, centerHz: null, sampleRateHz: null, rowsPerS: null, recording: false },
    live: { streamId: null, centerHz: null, bandwidthHz: null, bins: null, rowRateHz: null, view: null },
    focus: { kind: "none" },
    inventory: { tab: "confirmed", sort: { key: "freq", dir: 1 }, rows: {}, loadedAtS: null, error: null },
    selections: { list: [], sync: "" },
    outputs: [], time: { live: true },
    decode: { pipelineId: null, nodeId: null, frameSeq: null, fieldNodeId: null },
    nav: { gotoHz: null, seq: 0 },
    toast: { text: "", seq: 0 },
  };
}

// ---- actions (pure) ----

export const setMode = (mode: Mode) => (): Partial<AppState> => ({ mode });

const THEMES: readonly Theme[] = ["system", "dark", "light"];
export const cycleTheme = (s: AppState): Partial<AppState> => ({ theme: THEMES[(THEMES.indexOf(s.theme) + 1) % THEMES.length] });

/** Focus a signal; switches the inventory tab to the row's state so the row is visible. */
export const focusSignal = (id: string) => (s: AppState): Partial<AppState> => {
  const row = s.inventory.rows[id];
  const tab: InventoryTab | null = row && (row.state === "confirmed" || row.state === "candidate") ? row.state : null;
  return tab && tab !== s.inventory.tab
    ? { focus: { kind: "signal", id }, inventory: { ...s.inventory, tab } }
    : { focus: { kind: "signal", id } };
};

export const focusSelection = (id: string) => (): Partial<AppState> => ({ focus: { kind: "selection", id } });

export const toggleReview = (s: AppState): Partial<AppState> => ({ review: { ...s.review, open: !s.review.open } });
export const openReview = (tab: ReviewTab, region: ReviewSlice["region"] = null) => (): Partial<AppState> => ({ review: { open: true, tab, region } });

export const goLive = (): Partial<AppState> => ({ time: { live: true } });
export const reviewAt = (tS: number) => (): Partial<AppState> => (Number.isFinite(tS) ? { time: { live: false, tS } } : {});

export const requestGoto = (hz: number) => (s: AppState): Partial<AppState> => ({ nav: { gotoHz: hz, seq: s.nav.seq + 1 } });

export const toast = (text: string) => (s: AppState): Partial<AppState> => ({ toast: { text, seq: s.toast.seq + 1 } });

/** Adds or replaces an Outputs entry by id. */
export const upsertOutput = (e: OutputEntry) => (s: AppState): Partial<AppState> => {
  const i = s.outputs.findIndex((o) => o.id === e.id);
  return { outputs: i < 0 ? [...s.outputs, e] : s.outputs.map((o, j) => (j === i ? e : o)) };
};

export const removeOutput = (id: string) => (s: AppState): Partial<AppState> =>
  (s.outputs.some((o) => o.id === id) ? { outputs: s.outputs.filter((o) => o.id !== id) } : {});
