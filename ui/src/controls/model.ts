// `/api/control/state` (T-050) → what the panel enables, offers and says (T-051). Pure:
// unit-tested in ui/test/controls.test.ts, including against a state body captured from
// `hk serve --replay` (ui/test/control_state_replay.json).

export interface GainStageCap { name: string; min_db: number; max_db: number; step_db: number }

/** A device's selectable baseband (anti-alias) filter bandwidths (T-067), Hz. */
export type BasebandFilterCap = { min_hz: number; max_hz: number } | { values_hz: number[] };

export interface DeviceCaps {
  /** The front end's provenance `device_id` (T-343), e.g. `hackrf:<serial>`; null when the source
   * reports no identity — "nothing said", never a placeholder. */
  device_id: string | null;
  driver: string;
  kind: "hardware" | "replay";
  controllable: boolean;
  frequency_ranges_hz: [number, number][];
  sample_rates_hz: { min: number; max: number } | { values: number[] };
  /** Centre-frequency granularity (T-341): `"uniform"` with a step, or `"unknown"` when the source
   * cannot say. Three-valued like the bias tee, and for the same reason — "nothing said" read as
   * 1 Hz would offer centres the radio cannot reach. */
  tuning_step: "uniform" | "unknown";
  /** The step in Hz, or null when `tuning_step` is `"unknown"`. Never default it to 1. */
  tuning_step_hz: number | null;
  gain_stages: GainStageCap[];
  bias_tee: boolean;
  baseband_filter: BasebandFilterCap | null;
  adc_bits: number;
  tx_capable_hardware: boolean;
}

/**
 * Antenna-port bias-tee state from `/api/control/state` (T-325): three states, never a bool.
 * `"unknown"` means nothing reported one — it must never be rendered as off, since a bias tee left
 * on into a passive or DC-shorted port is a hardware hazard.
 */
export type BiasTeeState = "unknown" | "off" | "on";

export interface Tuning {
  center_hz: number; sample_rate_hz: number; gains: Record<string, number>; bias_tee: BiasTeeState;
  baseband_filter_hz: number | null;
}
/** `/api/control/state`'s `display`. No `paused` (T-347): that was run-wide state every client
 * shared, so one browser's Pause froze all of them. Holding the view is the client's own time
 * cursor (`state.time`), never a server setting. */
export interface Display { fft_size: number; averaging: number; rows_per_s: number; window: string }
/** `/api/control/state`'s `display_limits` (T-067): the UI stops hard-coding hk-pipeline's `DISPLAY_*` bounds. */
export interface DisplayLimits {
  fft_size_min: number; fft_size_max: number; averaging_max: number;
  rows_per_s_min: number; rows_per_s_max: number; windows: string[];
}
export interface Recording {
  active: boolean; id: string | null; label: string | null; center_hz: number | null; sample_rate_hz: number | null;
  samples: number; lost_samples: number; max_s: number; stored: boolean; ended: string | null;
}
export interface Run {
  live: boolean; content_class: string; content_permitted: boolean; center_hz: number; sample_rate_hz: number;
  segment: number; replumbing: boolean; finished: boolean; display: Display; recording: Recording;
}
/** `/api/control/scan`'s `plan` (T-452): the sweep as compiled against this front end. */
export interface ScanPlan {
  f_lo_hz: number; f_hi_hz: number; dwell_s: number;
  /** Whether the dwell is inside the 10-30 s the survey is sized for. A dwell outside it still
   * runs — configurable, never clamped — and this is what lets the panel say it is unusual. */
  recommended_dwell: boolean;
  /** The span in force: the pass is tiled at the window the run is actually capturing. */
  sample_rate_hz: number;
  steps: number;
  warnings: string[];
}
/** What one pass costs and buys (T-406's own pricing, served by T-452). */
export interface ScanBudget {
  steps: number; dwell_s: number; pass_s: number; revisit_s: number;
  span_hz: number; step_span_hz: number;
  /** `dwell / pass`: the honest headline of the trade, not a detection probability. */
  duty: number;
  /** T-406's sentence with this plan's numbers in it: what the pass catches and what it does not. */
  statement: string;
}
export interface ScanProgress {
  step: number; steps: number; pass: number; steps_done: number;
  center_hz: number | null; started_s: number; step_started_s: number | null; next_step_in_s: number;
}
/** Why the sweep stopped stepping (T-452). `to` is a device-action name, or `"step_failed"`. */
export interface ScanYielded { to: string; at_s: number; step: number; detail: string }
export interface ScanState {
  state: "idle" | "running" | "yielded";
  available: boolean;
  unavailable_reason: string | null;
  plan: ScanPlan | null;
  budget: ScanBudget | null;
  progress: ScanProgress | null;
  yielded: ScanYielded | null;
}

export interface ControlState {
  live: boolean;
  device: DeviceCaps | null;
  tuning: Tuning | null;
  run: Run | null;
  /** T-452: the survey sweep, beside the tuning it moves. `null` when nothing can sweep this
   * source (a replay), so the panel disables the control with a reason rather than hiding it. */
  scan: ScanState | null;
  display_limits: DisplayLimits | null;
  transmit: { available: false; reason: string };
  audit: boolean;
}

/**
 * Fallback display limits (T-067), used only until the first `/api/control/state` answers: after
 * that the panel reads `state.display_limits` instead of hard-coding hk-pipeline's `DISPLAY_*`.
 */
export const FALLBACK_LIMITS: DisplayLimits = {
  fft_size_min: 64, fft_size_max: 65536, averaging_max: 100,
  rows_per_s_min: 0.5, rows_per_s_max: 200, windows: ["hann", "blackman-harris", "flat-top"],
};

/** FFT sizes offered: every power of two in the limits' range. */
export function fftSizeOptions(limits: DisplayLimits): number[] {
  const out: number[] = [];
  for (let n = limits.fft_size_min; n <= limits.fft_size_max; n *= 2) out.push(n);
  return out;
}

/** Waterfall speeds offered: the standard steps, clamped into the limits' range (always including the max). */
const STANDARD_ROW_RATES = [0.5, 1, 2, 5, 10, 25, 50, 100, 200];
export function rowRateOptions(limits: DisplayLimits): number[] {
  const out = STANDARD_ROW_RATES.filter((r) => r >= limits.rows_per_s_min && r <= limits.rows_per_s_max);
  if (!out.includes(limits.rows_per_s_max)) out.push(limits.rows_per_s_max);
  return out.sort((a, b) => a - b);
}

/** A window name into a human label ("hann" -> "Hann", "blackman-harris" -> "Blackman-Harris"). */
export function windowLabel(name: string): string {
  return name.split("-").map((w) => w.charAt(0).toUpperCase() + w.slice(1)).join("-");
}

/** Content-forbidding classes gate spectrum at this many rows per second (hk-pipeline class.rs). */
export const GATED_ROWS_PER_S = 50;

/** Rates offered for a continuous range (HackRF-friendly, integer decimation from 20 Msps where possible). */
const STANDARD_RATES = [2e6, 2.4e6, 4e6, 5e6, 8e6, 10e6, 12.5e6, 16e6, 20e6];

/** Sample-rate (span) choices from the capabilities, always including the current rate. */
export function rateOptions(caps: DeviceCaps | null, currentHz: number | undefined): number[] {
  let out: number[] = [];
  if (caps) {
    const r = caps.sample_rates_hz;
    out = "values" in r ? [...r.values] : STANDARD_RATES.filter((v) => v >= r.min && v <= r.max);
    if (!("values" in r) && !out.length) out = [r.min, r.max].filter((v, i, a) => a.indexOf(v) === i);
  }
  if (currentHz !== undefined && Number.isFinite(currentHz) && !out.includes(currentHz)) out.push(currentHz);
  return out.sort((a, b) => a - b);
}

/** Known stage names get a friendly label; any other device's stages keep their own name. */
const STAGE_LABELS: Record<string, string> = { lna: "LNA", vga: "VGA", amp: "RF amp", if: "IF", mix: "Mixer", pga: "PGA", tuner: "Tuner" };

export interface GainControl {
  name: string;
  label: string;
  min: number;
  max: number;
  step: number;
  value: number;
  /** A two-position stage (e.g. the HackRF amp, 0 or 11 dB) renders as a switch. */
  toggle: boolean;
}

export function gainControls(caps: DeviceCaps | null, tuning: Tuning | null): GainControl[] {
  return (caps?.gain_stages ?? []).map((s) => {
    const step = s.step_db > 0 ? s.step_db : 0.5;
    const v = tuning?.gains[s.name];
    return {
      name: s.name,
      label: STAGE_LABELS[s.name.toLowerCase()] ?? s.name,
      min: s.min_db,
      max: s.max_db,
      step,
      value: typeof v === "number" && Number.isFinite(v) ? v : s.min_db,
      toggle: s.step_db > 0 && Math.abs((s.max_db - s.min_db) / s.step_db - 1) < 1e-9,
    };
  });
}

/** "restricted-paging: metadata only", "unrestricted: content allowed". */
export function classLabel(cls: string, permitted: boolean): string {
  if (cls === "own-key-decrypted") return `${cls}: local only`;
  return `${cls}: ${permitted ? "content allowed" : "metadata only"}`;
}

export interface Gate { enabled: boolean; reason: string }

export interface PanelModel {
  /** Centre, rate, gains, bias tee, baseband filter. */
  device: Gate;
  /** FFT size, averaging, speed, window (the run's shared display settings). */
  display: Gate;
  /** Record start (stop stays possible while one is active). */
  record: Gate & { active: boolean };
  /** A re-plumb is running (server) or a device request is in flight (this page). */
  busy: boolean;
  classText: string;
  gated: boolean;
  segment: number | null;
  rates: number[];
  gains: GainControl[];
  /** `unknown` renders as an indeterminate box, never as an unticked (= off) one (T-325). */
  biasTee: { available: boolean; on: boolean; unknown: boolean };
  basebandFilter: { available: boolean; options: number[]; value: number | null };
  frequencyRanges: [number, number][];
  /** Display setting bounds (T-067): from `state.display_limits`, else [`FALLBACK_LIMITS`]. */
  limits: DisplayLimits;
}

/** Baseband filter choices (T-067) from the device's discrete list, or 9 log-spaced steps of a
 * continuous range; the current value is always included. */
export function basebandFilterOptions(cap: BasebandFilterCap | null, currentHz: number | undefined): number[] {
  let out: number[] = [];
  if (cap) {
    if ("values_hz" in cap) out = [...cap.values_hz];
    else {
      const { min_hz, max_hz } = cap, steps = 8;
      out = min_hz > 0
        ? Array.from({ length: steps + 1 }, (_, i) => min_hz * (max_hz / min_hz) ** (i / steps))
        : Array.from({ length: steps + 1 }, (_, i) => min_hz + ((max_hz - min_hz) * i) / steps);
    }
  }
  if (currentHz !== undefined && Number.isFinite(currentHz) && !out.includes(currentHz)) out.push(currentHz);
  return out.sort((a, b) => a - b);
}

const on: Gate = { enabled: true, reason: "" };
const off = (reason: string): Gate => ({ enabled: false, reason });

/**
 * The panel's state. `pending`: this page has a device request in flight (no double submit).
 * Replay (no live source): device controls off with the `not_live` reason; display, recording and
 * bookmarks stay on.
 */
export function panelModel(s: ControlState, pending = false): PanelModel {
  const run = s.run;
  const live = s.live && !!s.device && !!s.tuning;
  let device: Gate = on, display: Gate = on;
  if (!s.audit) device = display = off("control disabled: this server has no audit log");
  else if (!run) display = off("no running pipeline on this server");
  else if (run.finished) device = display = off("the run has finished");
  if (device.enabled) {
    if (!live) device = off("not_live: this server replays a recording; device settings need a live source");
    else if (!s.device!.controllable) device = off(`${s.device!.driver} is not controllable`);
    else if (run?.replumbing || pending) device = off("retuning: a re-plumb can take up to 30 s");
  }
  const rec = run?.recording;
  let record: Gate = display.enabled ? on : display;
  if (record.enabled && run && !run.content_permitted) {
    record = off(`recording refused under ${run.content_class}: its content may not be stored`);
  }
  if (rec?.active && record.enabled) record = off("a recording is running");
  return {
    device,
    display,
    record: { ...record, active: !!rec?.active },
    busy: !!run?.replumbing || pending,
    classText: run ? classLabel(run.content_class, run.content_permitted) : "no run",
    gated: !!run && !run.content_permitted,
    segment: run ? run.segment : null,
    rates: rateOptions(s.device, s.tuning?.sample_rate_hz ?? run?.sample_rate_hz),
    gains: live ? gainControls(s.device, s.tuning) : [],
    biasTee: {
      available: live && !!s.device!.bias_tee,
      on: s.tuning?.bias_tee === "on",
      unknown: s.tuning?.bias_tee !== "on" && s.tuning?.bias_tee !== "off",
    },
    basebandFilter: {
      available: live && !!s.device!.baseband_filter,
      options: basebandFilterOptions(s.device?.baseband_filter ?? null, s.tuning?.baseband_filter_hz ?? undefined),
      value: s.tuning?.baseband_filter_hz ?? null,
    },
    frequencyRanges: s.device?.frequency_ranges_hz ?? [],
    limits: s.display_limits ?? FALLBACK_LIMITS,
  };
}

/** What the sweep control shows and offers (T-452). Pure; tested in ui/test/controls.test.ts. */
export interface ScanPanelModel {
  /** Whether a sweep can be started at all, and why not when it cannot. */
  gate: Gate;
  state: "idle" | "running" | "yielded";
  /** The primary button: start a new sweep, or resume the one that yielded. */
  primary: { label: string; action: "start" | "resume"; enabled: boolean };
  stopEnabled: boolean;
  /** The arithmetic the user is committing to, from `proposed` (before) or `budget` (during).
   * Empty until something has been priced — never a guess. */
  commitment: string;
  /** T-406's own sentence about what the pass catches and what it does not; "" when unpriced. */
  statement: string;
  /** Where the sweep is, while it is somewhere. */
  progressText: string;
  /** Why it stopped, when it has; "" otherwise. A yield is never silent. */
  yieldText: string;
  /** Clipping and the like, plus a dwell outside the range the survey is sized for. */
  notes: string[];
}

/** A sweep's length in words, for the commitment line. */
function passText(s: number): string {
  if (s < 90) return `${Math.round(s)} s`;
  if (s < 5400) return `${Math.round(s / 60)} min`;
  return `${(s / 3600).toFixed(1)} h`;
}

/**
 * The one line that says what starting this sweep costs: how many steps, how long a pass, and how
 * little of the time any one band is actually being listened to.
 *
 * This is the whole reason the control prices before it starts. A 6 GHz sweep at a 15 s dwell is
 * ~80 minutes at a 0.25 % duty, and a user who reads that first is making a different decision
 * from one who finds out by waiting.
 */
export function commitmentText(b: ScanBudget): string {
  const duty = b.duty >= 0.01 ? `${(b.duty * 100).toFixed(1)} %` : `${(b.duty * 100).toFixed(3)} %`;
  return `${b.steps} steps × ${b.dwell_s} s = ${passText(b.pass_s)} per pass; `
    + `each band is heard ${b.dwell_s} s in every ${passText(b.revisit_s)} (duty ${duty}).`;
}

/**
 * The sweep control's state.
 *
 * `proposed` is the budget of what the *inputs currently say*, from `GET /api/control/scan?…` —
 * it is what makes the commitment line describe the sweep about to be started rather than the one
 * already running. `pending`: a start/stop request is in flight on this page.
 */
export function scanPanelModel(
  s: ControlState,
  proposed: { plan: ScanPlan; budget: ScanBudget } | null = null,
  pending = false,
): ScanPanelModel {
  const scan = s.scan;
  let gate: Gate = on;
  if (!s.audit) gate = off("control disabled: this server has no audit log");
  else if (!scan) gate = off("not_live: this server replays a recording; there is no front end to sweep");
  else if (!scan.available) gate = off(scan.unavailable_reason ?? "this front end cannot be swept");
  else if (s.run?.finished) gate = off("the run has finished");
  const state = scan?.state ?? "idle";
  const resuming = state === "yielded";
  const shown = state === "idle" ? proposed?.budget ?? null : scan?.budget ?? null;
  const notes: string[] = [];
  const plan = state === "idle" ? proposed?.plan ?? null : scan?.plan ?? null;
  if (plan) {
    notes.push(...plan.warnings);
    if (!plan.recommended_dwell) {
      notes.push(`a ${plan.dwell_s} s dwell is outside the 10-30 s the survey is sized for; the pass is linear in it`);
    }
  }
  const p = scan?.progress ?? null;
  const progressText = p && scan?.plan
    ? `step ${p.step + 1} of ${p.steps} · pass ${p.pass + 1} · ${p.steps_done} steps done`
      + (p.center_hz !== null ? ` · tuned ${(p.center_hz / 1e6).toFixed(3)} MHz` : "")
    : "";
  return {
    gate,
    state,
    primary: {
      label: resuming ? "Resume sweep" : "Start sweep",
      action: resuming ? "resume" : "start",
      enabled: gate.enabled && state !== "running" && !pending,
    },
    stopEnabled: gate.enabled && state !== "idle" && !pending,
    commitment: shown ? commitmentText(shown) : "",
    statement: shown?.statement ?? "",
    progressText,
    yieldText: scan?.yielded
      ? `${scan.yielded.detail} Resume to continue, or stop it.`
      : "",
    notes,
  };
}

/** What the sweep inputs ask for, on the wire. Omitted fields take the server's own defaults:
 * everything this front end can tune, and T-406's 15 s dwell. */
export interface ScanQuery { f_lo_hz?: number; f_hi_hz?: number; dwell_s?: number }

/**
 * The scan request the range/dwell boxes mean. Megahertz in the boxes, hertz on the wire.
 *
 * **A half-stated range is not a range**: `from` without `to` sends neither, so the server is
 * never handed a half request and this layer never invents the other end. Blank boxes mean "the
 * whole front end", which is a real answer and not a missing one.
 */
export function scanQueryFrom(input: { loMHz: string; hiMHz: string; dwellS: string }): ScanQuery {
  const mhz = (s: string): number | null => {
    const v = Number(s);
    return s.trim() !== "" && Number.isFinite(v) ? v * 1e6 : null;
  };
  const [lo, hi] = [mhz(input.loMHz), mhz(input.hiMHz)];
  const dwell = Number(input.dwellS);
  const q: ScanQuery = {};
  if (lo !== null && hi !== null) { q.f_lo_hz = lo; q.f_hi_hz = hi; }
  if (input.dwellS.trim() !== "" && Number.isFinite(dwell) && dwell > 0) q.dwell_s = dwell;
  return q;
}

/** The `GET /api/control/scan` path that prices `q` without starting it; bare when `q` is empty. */
export function scanPreviewPath(q: ScanQuery): string {
  const search = new URLSearchParams(Object.entries(q).map(([k, v]) => [k, String(v)]));
  const s = search.toString();
  return s ? `/api/control/scan?${s}` : "/api/control/scan";
}

/** A notice when the run re-plumbed since `prev` (segment change), naming a class change. */
export function segmentNotice(prev: { segment: number; content_class: string } | null, run: Run | null): string | null {
  if (!prev || !run || run.segment === prev.segment) return null;
  const cls = run.content_class === prev.content_class
    ? `class unchanged (${run.content_class})`
    : `class ${prev.content_class} → ${classLabel(run.content_class, run.content_permitted)}`;
  return `re-plumbed: segment ${prev.segment} → ${run.segment}; ${cls}`;
}

/** One-line recording status. */
export function recordingText(r: Recording | undefined, sampleRateHz: number | undefined): string {
  if (!r || (!r.active && !r.id)) return "not recording";
  const s = sampleRateHz && sampleRateHz > 0 ? ` (${(r.samples / sampleRateHz).toFixed(1)} s)` : "";
  const lost = r.lost_samples ? ` · lost ${r.lost_samples}` : "";
  if (r.active) return `recording${r.label ? ` "${r.label}"` : ""}: ${r.samples} samples${s}${lost}`;
  return `last recording${r.label ? ` "${r.label}"` : ""}: ${r.samples} samples${s}${lost}` +
    `${r.stored ? " · stored" : " · not stored"}${r.ended ? ` · ${r.ended}` : ""}`;
}
