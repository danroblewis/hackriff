// Shell state (ADR-0013 §3.1). Owner: T-150 (shell). Top-level keys: mode, theme, conn, device,
// nav, toast. `device` is T-150's alone (the top bar's reduction of `/api/control/state`); T-155's
// Device tab keeps the full control state it needs in its own review slice, never here.
import type { CenterGrid, FftBounds } from "../navigation";
import type { CaptureState } from "../controls/model";
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
  /** T-508: `run.capture` — whether the front end is delivering (null before the state loads or
   * with no run). An older server that does not send it is read from `finished`. */
  capture: CaptureState | null;
  /** T-508: `run.capture_note`, the backend's own cause while recovering or after a failed end. */
  captureNote: string | null;
  centerHz: number | null; sampleRateHz: number | null; rowsPerS: number | null; recording: boolean;
  /** The front end's provenance `device_id` (T-343), e.g. `hackrf:<serial>`; null when the source
   * reports no identity. A retune is a device action recorded against this id, so the UI names the
   * radio it is about to move. Null means *nothing said*, never "some default device". */
  deviceId: string | null;
  /** The centre axis of the achievable grid (T-341): the tunable bounds and the tuning step, from
   * `/api/control/state`'s `device`. Null before the state loads or on a run with no device; a
   * `center_step_hz` of null means the source cannot state a step, and then **nothing snaps**. */
  centerGrid: CenterGrid | null;
  /**
   * The FFT axis of `/api/control/state`'s `display_limits` (T-418): the bounds a longer transform
   * may be asked for within, and the other half of "navigation is discretized to achievable states"
   * — a narrow selection's detail comes from the transform, so the transform has its own ladder.
   *
   * Null before the state loads or on a server with no running pipeline, and then **nothing raises
   * the resolution**: the same discipline as a null `center_step_hz`. Not knowing the bound is not
   * permission to invent one, and a client that guessed would be asking for a size the server may
   * reject on a device it has not been told about.
   */
  fftBounds: FftBounds | null;
}

/** One-shot navigation requests from the top bar (Go to), consumed by T-151/T-152. */
export interface NavSlice { gotoHz: number | null; seq: number }

export interface ShellState {
  mode: Mode; theme: Theme; conn: ConnSlice; device: DeviceSlice; nav: NavSlice;
  toast: { text: string; seq: number };
  /** Open anomaly count for the Review button's badge (`GET /api/anomalies?status=open`, §4.1). */
  openAlarms: number;
  /** T-391: whether the Capture panel's header disclosure is collapsed. A per-viewer display
   * preference like `mode`/`theme` — persisted the same way, never something the backend needs — so
   * folding the panel away survives a reload instead of resetting every time. */
  captureCollapsed: boolean;
}

/** Per-viewer preferences kept in localStorage (never state that must persist). */
export interface Prefs { mode: Mode; theme: Theme; captureCollapsed: boolean }

export function parsePrefs(raw: string | null): Prefs {
  const d: Prefs = { mode: "explore", theme: "system", captureCollapsed: false };
  if (!raw) return d;
  try {
    const p = JSON.parse(raw) as Partial<Prefs>;
    return {
      mode: p.mode === "decode" || p.mode === "history" ? p.mode : "explore",
      theme: p.theme === "dark" || p.theme === "light" ? p.theme : "system",
      captureCollapsed: p.captureCollapsed === true,
    };
  } catch {
    return d;
  }
}

export const shellInitial = (prefs: Prefs): ShellState => ({
  mode: prefs.mode, theme: prefs.theme,
  conn: { api: "connecting", spectrum: "idle", message: "" },
  device: { loaded: false, live: false, finished: false, capture: null, captureNote: null, contentClass: null, centerHz: null, sampleRateHz: null, rowsPerS: null, recording: false, deviceId: null, centerGrid: null, fftBounds: null },
  nav: { gotoHz: null, seq: 0 },
  toast: { text: "", seq: 0 },
  openAlarms: 0,
  captureCollapsed: prefs.captureCollapsed,
});

export const setMode = (mode: Mode) => (): Partial<AppState> => ({ mode });

const THEMES: readonly Theme[] = ["system", "dark", "light"];
export const cycleTheme = (s: AppState): Partial<AppState> => ({ theme: THEMES[(THEMES.indexOf(s.theme) + 1) % THEMES.length] });

export const requestGoto = (hz: number) => (s: AppState): Partial<AppState> => ({ nav: { gotoHz: hz, seq: s.nav.seq + 1 } });

export const toast = (text: string) => (s: AppState): Partial<AppState> => ({ toast: { text, seq: s.toast.seq + 1 } });

/** T-391: fold or unfold the Capture panel's overview band. Pure state only — capture/index.ts
 * reads it back to hide the band and to toggle the layout class the panel's row size depends on. */
export const setCaptureCollapsed = (collapsed: boolean) => (): Partial<AppState> => ({ captureCollapsed: collapsed });
