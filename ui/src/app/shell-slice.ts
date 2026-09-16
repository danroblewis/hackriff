// Shell state (ADR-0013 §3.1). Owner: T-150 (shell). Top-level keys: mode, theme, conn, device,
// nav, toast. `device` is T-150's alone (the top bar's reduction of `/api/control/state`); T-155's
// Device tab keeps the full control state it needs in its own review slice, never here.
import type { CenterGrid } from "../navigation";
import type { AppState } from "./state";

/** T-264 (ADR-0017 TM-8): `history` is the durable all-time catalogue (workflow #3), a surface of
 * its own because Explore answers "what is here now" and is scoped to the viewed window. */
export type Mode = "explore" | "decode" | "history";
export type Theme = "system" | "dark" | "light";

export type ApiConn = "connecting" | "ok" | "offline" | "unauthorized";
export type StreamConn = "idle" | "connecting" | "live" | "reconnecting" | "unavailable";

export interface ConnSlice { api: ApiConn; spectrum: StreamConn; message: string }

/** `GET /api/control/state`, reduced to what the top bar shows (T-150 only). */
export interface DeviceSlice {
  loaded: boolean; live: boolean; finished: boolean; contentClass: string | null;
  centerHz: number | null; sampleRateHz: number | null; rowsPerS: number | null; recording: boolean;
  /** The front end's provenance `device_id` (T-343), e.g. `hackrf:<serial>`; null when the source
   * reports no identity. A retune is a device action recorded against this id, so the UI names the
   * radio it is about to move. Null means *nothing said*, never "some default device". */
  deviceId: string | null;
  /** The centre axis of the achievable grid (T-341): the tunable bounds and the tuning step, from
   * `/api/control/state`'s `device`. Null before the state loads or on a run with no device; a
   * `center_step_hz` of null means the source cannot state a step, and then **nothing snaps**. */
  centerGrid: CenterGrid | null;
}

/** One-shot navigation requests from the top bar (Go to), consumed by T-151/T-152. */
export interface NavSlice { gotoHz: number | null; seq: number }

export interface ShellState {
  mode: Mode; theme: Theme; conn: ConnSlice; device: DeviceSlice; nav: NavSlice;
  toast: { text: string; seq: number };
  /** Open anomaly count for the Review button's badge (`GET /api/anomalies?status=open`, §4.1). */
  openAlarms: number;
}

/** Per-viewer preferences kept in localStorage (never state that must persist). */
export interface Prefs { mode: Mode; theme: Theme }

export function parsePrefs(raw: string | null): Prefs {
  const d: Prefs = { mode: "explore", theme: "system" };
  if (!raw) return d;
  try {
    const p = JSON.parse(raw) as Partial<Prefs>;
    return {
      mode: p.mode === "decode" || p.mode === "history" ? p.mode : "explore",
      theme: p.theme === "dark" || p.theme === "light" ? p.theme : "system",
    };
  } catch {
    return d;
  }
}

export const shellInitial = (prefs: Prefs): ShellState => ({
  mode: prefs.mode, theme: prefs.theme,
  conn: { api: "connecting", spectrum: "idle", message: "" },
  device: { loaded: false, live: false, finished: false, contentClass: null, centerHz: null, sampleRateHz: null, rowsPerS: null, recording: false, deviceId: null, centerGrid: null },
  nav: { gotoHz: null, seq: 0 },
  toast: { text: "", seq: 0 },
  openAlarms: 0,
});

export const setMode = (mode: Mode) => (): Partial<AppState> => ({ mode });

const THEMES: readonly Theme[] = ["system", "dark", "light"];
export const cycleTheme = (s: AppState): Partial<AppState> => ({ theme: THEMES[(THEMES.indexOf(s.theme) + 1) % THEMES.length] });

export const requestGoto = (hz: number) => (s: AppState): Partial<AppState> => ({ nav: { gotoHz: hz, seq: s.nav.seq + 1 } });

export const toast = (text: string) => (s: AppState): Partial<AppState> => ({ toast: { text, seq: s.toast.seq + 1 } });
