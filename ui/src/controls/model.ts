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
export interface Display { fft_size: number; averaging: number; rows_per_s: number; paused: boolean; window: string }
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
export interface ControlState {
  live: boolean;
  device: DeviceCaps | null;
  tuning: Tuning | null;
  run: Run | null;
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
  /** FFT size, averaging, speed, window, pause (server-side display). */
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
 * Replay (no live source): device controls off with the `not_live` reason; display, pause,
 * recording and bookmarks stay on.
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
