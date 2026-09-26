// Shell state (ADR-0013 §3.1). Owner: T-150 (shell). Top-level keys: mode, theme, conn, device,
// nav, toast. `device` is T-150's alone (the top bar's reduction of `/api/control/state`); T-155's
// Device tab keeps the full control state it needs in its own review slice, never here.
import type { CenterGrid, FftBounds } from "../navigation";
import type { AttachedDevice } from "../surface/panedevice";
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
  /**
   * **Every live front end this run holds** (T-1006), from `/api/control/state`'s `devices[]`
   * (T-511): `[]` on a replay, one entry on a single-SDR run, N when N are composed.
   *
   * Here rather than in the Device tab's own slice because it is not a device *setting* — it is the
   * list a **pane** picks its coverage selector from and the list a retune names a `device_id` out
   * of, so the surface needs it on the same 2 s poll as the rest of this slice. `deviceId` above
   * stays the singular default (null with several, because then there is no "the" device).
   */
  devices: readonly AttachedDevice[];
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
  /**
   * T-1007 (over T-511's `devices`): every live front end, for the ⋯ settings menu's list and for
   * the per-pane choice of whose coverage decides a pane's grey. `[]` on a replay, on a server that
   * does not send the list, or before the state loads — never a list invented from the singular
   * `device`, which is null exactly when the run holds more than one.
   *
   * Beside T-1006's `devices` (above), not instead of it: that list keeps only the ADDRESSABLE front
   * ends (a `device_id` a selector can name), which is what a pane pin and a retune need; this one
   * keeps every entry the server listed, with its kind, because the settings menu states each radio —
   * including one that reports no id ("this source reports no device id"). Named apart so the two
   * readings of one wire list cannot be confused (integration of T-1006 and T-1007).
   */
  frontEnds: readonly FrontEnd[];
  /** T-1009: every front end's sweep (`/api/control/state`'s `scans`), so a scan plan bound to a
   * chosen radio follows THAT radio's sweep — with two SDRs the default one's state says nothing
   * about a sweep started on the other. `[]` on a replay or before the state loads. */
  scans?: readonly ScanState[];
}

/** One front end as the shell reduces it: its identity, what it is, and what it is tuned to now. */
export interface FrontEnd {
  deviceId: string | null;
  driver: string;
  kind: "hardware" | "replay";
  centerHz: number | null;
  sampleRateHz: number | null;
}

/** One-shot navigation requests from the top bar (Go to), consumed by T-151/T-152. `gotoSpanHz`
 * (T-906) is the frequency span to show, when the request names one (a past survey's band); null
 * keeps the pane's own span. Either way this is view arithmetic: the surface snaps it to a realizable
 * pane (`PaneModel`'s clamp to the zoom floor and the device range) and never reaches a device route,
 * so a span wider than the instantaneous bandwidth is a zoom over history, not a retune.
 *
 * `gotoTS`/`gotoSpanS` (T-999) name the TIME half of the same request, when it has one (a past
 * survey's window): the instant to freeze the pane at, and, when the request names one, the time
 * span to show — `null` keeps the pane's own. This is carried on the SAME request/seq pair as the
 * frequency fields so one `nav` write moves both axes atomically; splitting them (as the drawer used
 * to, writing `capture-slice`'s `reviewAt` separately) lost the time half the moment the active
 * pane's own per-frame `mirror()` ran, because nothing read the store's `time` back into a pane. */
export interface NavSlice {
  gotoHz: number | null; gotoSpanHz: number | null;
  gotoTS: number | null; gotoSpanS: number | null;
  seq: number;
}

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
  device: { loaded: false, live: false, finished: false, capture: null, captureNote: null, contentClass: null, centerHz: null, sampleRateHz: null, rowsPerS: null, recording: false, deviceId: null, devices: [], centerGrid: null, fftBounds: null, frontEnds: [], scans: [] },
  nav: { gotoHz: null, gotoSpanHz: null, gotoTS: null, gotoSpanS: null, seq: 0 },
  toast: { text: "", seq: 0 },
  openAlarms: 0,
});

export const setMode = (mode: Mode) => (): Partial<AppState> => ({ mode });

const THEMES: readonly Theme[] = ["system", "dark", "light"];
export const cycleTheme = (s: AppState): Partial<AppState> => ({ theme: THEMES[(THEMES.indexOf(s.theme) + 1) % THEMES.length] });

/** `time`, when given (T-999), names the (t0, t1) window a past-survey Go-to also asks for — a
 * single request carrying both axes, so the surface's one `nav` subscriber moves the pane's
 * frequency AND time together and nothing later in the frame (`mirror()`) can overwrite the half
 * that was written some other way.
 *
 * `gotoTS` is the window's MIDPOINT `(t0S + t1S) / 2`, not its end: `PaneModel.goTo` takes a
 * CENTRE (`centre/surface.ts` passes `t.tS` straight through as `centerNs`), so a `gotoTS` of `t1S`
 * would land the pane with the whole survey window in the OLDER half of the frame and the newer
 * half off-screen — a real gate finding (review, 2026-09-25): "centres the pane on the survey's end
 * time t1 ... so half the window is off-screen". The midpoint is what actually shows `[t0S, t1S]`
 * centred, matching `gotoSpanS = t1S - t0S`. */
export const requestGoto = (hz: number, spanHz?: number, time?: { t0S: number; t1S: number }) =>
  (s: AppState): Partial<AppState> => ({
    nav: {
      gotoHz: hz, gotoSpanHz: spanHz !== undefined && Number.isFinite(spanHz) && spanHz > 0 ? spanHz : null,
      gotoTS: time && Number.isFinite(time.t0S) && Number.isFinite(time.t1S) ? (time.t0S + time.t1S) / 2 : null,
      gotoSpanS: time && Number.isFinite(time.t1S) && Number.isFinite(time.t0S) && time.t1S > time.t0S ? time.t1S - time.t0S : null,
      seq: s.nav.seq + 1,
    },
  });

/** The pane frequency window a go-to request asks for (T-906): its centre and, when it names one,
 * its span — otherwise the pane keeps `currentSpanHz`. Null when there is nowhere to go. The caller
 * hands this to `PaneModel.setFreq`, whose normalisation snaps it to a realizable window. */
export function gotoWindow(nav: NavSlice, currentSpanHz: number): { centerHz: number; spanHz: number } | null {
  if (nav.gotoHz === null || !Number.isFinite(nav.gotoHz)) return null;
  return { centerHz: nav.gotoHz, spanHz: nav.gotoSpanHz ?? currentSpanHz };
}

/** The pane time window a go-to request asks for (T-999): the instant to freeze the pane at and,
 * when the request names one, the span to show — otherwise the pane keeps its own. Null when the
 * request named no time (a plain frequency Go-to, which leaves the pane's time alone). */
export function gotoTimeWindow(nav: NavSlice): { tS: number; spanS: number | null } | null {
  if (nav.gotoTS === null || !Number.isFinite(nav.gotoTS)) return null;
  return { tS: nav.gotoTS, spanS: nav.gotoSpanS };
}

export const toast = (text: string) => (s: AppState): Partial<AppState> => ({ toast: { text, seq: s.toast.seq + 1 } });

