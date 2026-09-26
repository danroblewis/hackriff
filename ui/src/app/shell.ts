// MUI shell (skeleton, T-149; T-150 owns it from here): top bar (brand, Explore/Decode toggle,
// device + recording state, centre/span readouts, Go to, Review, theme — in Explore these float over
// the map instead, T-993 `chrome/top-chrome.ts`), view switching, theme
// stamping, toast, the token dialog, and the `GET /api/control/state` poll feeding `device`.
import { formatFrequency, parseFrequency } from "../controls/freq";
import type { ControlState } from "../controls/model";
import { placeTopChrome } from "./chrome/top-chrome";
import type { AppContext } from "./context";
import { byId, h } from "./dom";
import { apiConnFor, startPoll, storeToken } from "./net";
import { cycleTheme, requestGoto, setMode, toast, toggleReview, type AppState, type Mode, type Prefs } from "./state";

export const PREFS_KEY = "hk-mui-prefs";

/** The page element each mode shows. History (T-264, ADR-0017 TM-8) is a surface of its own beside
 * Explore and Decode, because the all-time record is a different question from "what is here now". */
export const VIEWS: Record<Mode, string> = {
  explore: "view-explore",
  decode: "view-decode",
  history: "view-history",
};

/** The `device` slice from a control-state answer (pure; tested). */
export function deviceFrom(cs: ControlState): AppState["device"] {
  const run = cs.run;
  return {
    loaded: true, live: !!cs.live, finished: !!run?.finished, contentClass: run?.content_class ?? null,
    capture: run ? run.capture ?? (run.finished ? "ended" : "running") : null,
    captureNote: run?.capture_note ?? null,
    centerHz: cs.tuning?.center_hz ?? run?.center_hz ?? null,
    sampleRateHz: cs.tuning?.sample_rate_hz ?? run?.sample_rate_hz ?? null,
    rowsPerS: run?.display?.rows_per_s ?? null, recording: !!run?.recording?.active,
    deviceId: cs.device?.device_id ?? null,
    // T-1006: every live front end, reduced to what a pane's device pill, its picker and a retune's
    // `device_id` need. An entry reporting NO identity is dropped rather than carried as a nameless
    // one: the server holds at most one such handle and it cannot be addressed by any selector, so a
    // row offering to pin a pane to it would offer an act that cannot be performed. On a run with
    // only that one front end the pane stays on `any`, the retune sends no selector, and the route's
    // own default is the unchanged single-device behaviour.
    devices: (cs.devices ?? [])
      .filter((d): d is typeof d & { device_id: string } => typeof d.device_id === "string" && d.device_id.length > 0)
      .map((d) => ({
        id: d.device_id,
        driver: d.device?.driver ?? "",
        centerHz: d.tuning?.center_hz ?? null,
        sampleRateHz: d.tuning?.sample_rate_hz ?? null,
      })),
    // T-341: the centre axis of the achievable grid, straight from the device's capabilities. A
    // `tuning_step_hz` of null is "the source cannot say" — carried through as null, never
    // defaulted to 1 Hz, so a snap against it refuses rather than inventing a centre.
    centerGrid: cs.device
      ? { ranges_hz: cs.device.frequency_ranges_hz, center_step_hz: cs.device.tuning_step_hz ?? null }
      : null,
    // T-418: the transform's own ladder, straight from `display_limits`. Null when this server has
    // no running pipeline to report one — and then nothing raises the resolution, rather than a
    // guess at a bound the server never stated.
    fftBounds: cs.display_limits
      ? { fft_size_min: cs.display_limits.fft_size_min, fft_size_max: cs.display_limits.fft_size_max }
      : null,
    // T-1007 (over T-511): the front ends themselves, in composition order, for the ⋯ settings
    // menu's list and the per-pane choice of whose coverage decides its grey. An absent list is `[]`
    // — never one entry synthesised from the singular `device`, which is null precisely when the run
    // holds more than one radio, and that is the case the list exists for. (`frontEnds`, beside
    // T-1006's addressable `devices` above — see `DeviceSlice.frontEnds`.)
    frontEnds: (cs.devices ?? []).map((d) => ({
      deviceId: d.device_id ?? d.device?.device_id ?? null,
      driver: d.device?.driver ?? "unknown front end",
      kind: d.device?.kind ?? "hardware",
      centerHz: d.tuning?.center_hz ?? null,
      sampleRateHz: d.tuning?.sample_rate_hz ?? null,
    })),
  };
}

export function mountShell(ctx: AppContext) {
  const { store, client } = ctx;
  const root = document.documentElement;

  // Mode toggle
  document.querySelectorAll<HTMLButtonElement>(".mode[data-mode]").forEach((b) =>
    b.addEventListener("click", () => store.set(setMode(b.dataset.mode as Mode))));
  store.select((s) => s.mode, (mode) => {
    document.querySelectorAll<HTMLButtonElement>(".mode[data-mode]").forEach((b) => b.setAttribute("aria-pressed", String(b.dataset.mode === mode)));
    for (const [m, id] of Object.entries(VIEWS)) byId(id)!.hidden = mode !== m;
    // T-993: Explore has no top bar — its controls float over the map; Decode/History get them back.
    placeTopChrome(mode === "explore");
  }, { immediate: true });

  // Theme: "system" stamps nothing (prefers-color-scheme decides); dark/light stamp data-theme.
  byId("theme-btn")!.addEventListener("click", () => store.set(cycleTheme));
  store.select((s) => s.theme, (t) => {
    if (t === "system") delete root.dataset.theme; else root.dataset.theme = t;
    byId("theme-btn")!.textContent = `Theme: ${t}`;
  }, { immediate: true });

  // Per-viewer prefs (a convenience only; storage may be unavailable).
  store.select((s) => `${s.mode}|${s.theme}`, () => {
    const { mode, theme } = store.get();
    try { localStorage.setItem(PREFS_KEY, JSON.stringify({ mode, theme } satisfies Prefs)); } catch { /* storage unavailable */ }
  });

  // Review drawer toggle (T-155 fills the drawer)
  byId("review-btn")!.addEventListener("click", () => store.set(toggleReview));
  store.select((s) => s.review.open, (open) => {
    byId("review")!.hidden = !open;
    byId("review-btn")!.setAttribute("aria-pressed", String(open));
  }, { immediate: true });

  // Go to: parse only (UI input formatting); T-151/T-152 act on nav.gotoHz.
  byId<HTMLFormElement>("goto-form")!.addEventListener("submit", (e) => {
    e.preventDefault();
    const input = byId<HTMLInputElement>("goto")!;
    const hz = parseFrequency(input.value, store.get().device.centerHz ?? undefined);
    if (hz === null) { store.set(toast("Enter a frequency, e.g. 101.3 MHz.")); return; }
    if (store.get().mode !== "explore") store.set(setMode("explore"));
    store.set(requestGoto(hz));
  });

  // Device and readouts
  store.select((s) => s.device, (d) => {
    byId("device-label")!.textContent = !d.loaded ? "connecting…"
      : d.capture === "recovering" ? "capture restarting…"
      : d.finished ? "capture stopped" : d.live ? "live" : "replay";
    byId("device")!.dataset.state = !d.loaded ? "connecting"
      : d.capture === "recovering" ? "recovering"
      : d.live && !d.finished ? "live" : "idle";
    byId("ro-centre")!.textContent = d.centerHz === null ? "–" : formatFrequency(d.centerHz);
    byId("ro-span")!.textContent = d.sampleRateHz === null ? "–" : formatFrequency(d.sampleRateHz);
  }, { immediate: true });

  // Recording pill (§4.1; API GAP 1: no always-on buffer status, so this shows only a manual
  // recording in progress, from the same control-state poll as `device`).
  store.select((s) => s.device.recording, (rec) => {
    const el = byId("rec-pill")!;
    el.hidden = !rec;
    if (rec) el.replaceChildren(h("span", { class: "d", "aria-hidden": "true" }), document.createTextNode("Recording (manual)"));
  }, { immediate: true });

  // Review badge (§4.1): open anomaly count, polled independently of the drawer's own tabs (T-155).
  // T-993: the button keeps its icon and label (the label is hidden when it sits in the map's
  // top-right cluster as an icon button), so the count is its own badge and the accessible name
  // carries it wherever the button is.
  store.select((s) => s.openAlarms, (n) => {
    const btn = byId("review-btn")!;
    const badge = btn.querySelector<HTMLElement>(".rv-n");
    const count = n > 99 ? "99+" : String(n);
    if (badge) { badge.textContent = n > 0 ? count : ""; badge.hidden = n <= 0; }
    btn.setAttribute("aria-label", n > 0 ? `Review (${count})` : "Review");
  }, { immediate: true });
  startPoll(async () => {
    const r = await client.get<{ anomalies: readonly unknown[] }>("/api/anomalies?status=open&limit=100");
    store.set((s) => (s.openAlarms === r.anomalies.length ? {} : { openAlarms: r.anomalies.length }));
  }, 30_000);

  store.select((s) => s.conn, (c) => {
    const el = byId("conn")!;
    el.textContent = c.api === "unauthorized" ? "token needed" : c.api === "offline" ? "server unreachable" : c.spectrum === "live" ? "" : c.message;
    el.hidden = !el.textContent;
    byId("auth")!.hidden = c.api !== "unauthorized";
  }, { immediate: true });

  // Toast
  let toastTimer = 0;
  store.select((s) => s.toast.seq, () => {
    const el = byId("toast")!;
    el.textContent = store.get().toast.text;
    el.classList.add("show");
    clearTimeout(toastTimer);
    toastTimer = window.setTimeout(() => el.classList.remove("show"), 2800);
  });

  // Token dialog
  byId<HTMLFormElement>("auth")!.addEventListener("submit", (e) => {
    e.preventDefault();
    storeToken(byId<HTMLInputElement>("token-input")!.value.trim());
    location.reload();
  });

  startPoll(async () => {
    const cs = await client.get<ControlState>("/api/control/state");
    store.set((s) => ({ device: deviceFrom(cs), conn: s.conn.api === "ok" ? s.conn : { ...s.conn, api: "ok", message: "" } }));
  }, 2000, (e) => store.set((s) => ({ conn: { ...s.conn, ...apiConnFor(e) } })));
}
