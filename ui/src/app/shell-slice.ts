// Shell state (ADR-0013 §3.1). Owner: T-150 (shell). Top-level keys: mode, theme, conn, device,
// nav, toast. `device` is T-150's alone (the top bar's reduction of `/api/control/state`); T-155's
// Device tab keeps the full control state it needs in its own review slice, never here.
import type { CenterGrid, FftBounds } from "../navigation";
import type { CaptureState, ScanState } from "../controls/model";
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
  /** T-1008: `/api/control/state`'s `scan` (T-452) as served — the compact form, without windows —
   * so the map's scan overlay learns of a sweep and its progress from the poll that already runs.
   * Null on a replay (nothing can be swept) or before the state loads. */
  scan?: ScanState | null;
}

/** One-shot navigation requests from the top bar (Go to), consumed by T-151/T-152. `gotoSpanHz`
 * (T-906) is the frequency span to show, when the request names one (a past survey's band); null
 * keeps the pane's own span. Either way this is view arithmetic: the surface snaps it to a realizable
 * pane (`PaneModel`'s clamp to the zoom floor and the device range) and never reaches a device route,
 * so a span wider than the instantaneous bandwidth is a zoom over history, not a retune. */
export interface NavSlice { gotoHz: number | null; gotoSpanHz: number | null; seq: number }

export interface ShellState {
  mode: Mode; theme: Theme; conn: ConnSlice; device: DeviceSlice; nav: NavSlice;
  toast: { text: string; seq: number };
  /** Open anomaly count for the Review button's badge (`GET /api/anomalies?status=open`, §4.1). */
  openAlarms: number;
}

/** Per-viewer preferences kept in localStorage (never state that must persist). T-506 removed
 * `captureCollapsed` with the Capture panel it folded; a stored one is ignored, not an error. */
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
  device: { loaded: false, live: false, finished: false, capture: null, captureNote: null, contentClass: null, centerHz: null, sampleRateHz: null, rowsPerS: null, recording: false, deviceId: null, centerGrid: null, fftBounds: null },
  nav: { gotoHz: null, gotoSpanHz: null, seq: 0 },
  toast: { text: "", seq: 0 },
  openAlarms: 0,
});

export const setMode = (mode: Mode) => (): Partial<AppState> => ({ mode });

const THEMES: readonly Theme[] = ["system", "dark", "light"];
export const cycleTheme = (s: AppState): Partial<AppState> => ({ theme: THEMES[(THEMES.indexOf(s.theme) + 1) % THEMES.length] });

export const requestGoto = (hz: number, spanHz?: number) => (s: AppState): Partial<AppState> => ({
  nav: { gotoHz: hz, gotoSpanHz: spanHz !== undefined && Number.isFinite(spanHz) && spanHz > 0 ? spanHz : null, seq: s.nav.seq + 1 },
});

/** The pane frequency window a go-to request asks for (T-906): its centre and, when it names one,
 * its span — otherwise the pane keeps `currentSpanHz`. Null when there is nowhere to go. The caller
 * hands this to `PaneModel.setFreq`, whose normalisation snaps it to a realizable window. */
export function gotoWindow(nav: NavSlice, currentSpanHz: number): { centerHz: number; spanHz: number } | null {
  if (nav.gotoHz === null || !Number.isFinite(nav.gotoHz)) return null;
  return { centerHz: nav.gotoHz, spanHz: nav.gotoSpanHz ?? currentSpanHz };
}

export const toast = (text: string) => (s: AppState): Partial<AppState> => ({ toast: { text, seq: s.toast.seq + 1 } });

