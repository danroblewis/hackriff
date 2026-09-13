// `/api/control/state` (T-050) → what the panel enables, offers and says (T-051). Pure:
// unit-tested in ui/test/controls.test.ts, including against a state body captured from
// `hk serve --replay` (ui/test/control_state_replay.json).

export interface GainStageCap { name: string; min_db: number; max_db: number; step_db: number }

export interface DeviceCaps {
  driver: string;
  kind: "hardware" | "replay";
  controllable: boolean;
  frequency_ranges_hz: [number, number][];
  sample_rates_hz: { min: number; max: number } | { values: number[] };
  gain_stages: GainStageCap[];
  bias_tee: boolean;
  adc_bits: number;
  tx_capable_hardware: boolean;
}

export interface Tuning { center_hz: number; sample_rate_hz: number; gains: Record<string, number>; bias_tee: boolean | null }
export interface Display { fft_size: number; averaging: number; rows_per_s: number; paused: boolean }
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
  transmit: { available: false; reason: string };
  audit: boolean;
}

/** Display limits (hk-pipeline `DISPLAY_*`; not in the state body, see ui/CONTROLS.md). */
export const FFT_SIZES = Array.from({ length: 11 }, (_, i) => 64 << i); // 64 .. 65536
export const AVERAGING_MAX = 100;
export const ROW_RATES = [0.5, 1, 2, 5, 10, 25, 50, 100, 200];
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
  /** Centre, rate, gains, bias tee. */
  device: Gate;
  /** FFT size, averaging, speed, pause (server-side display). */
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
  biasTee: { available: boolean; on: boolean };
  frequencyRanges: [number, number][];
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
    biasTee: { available: live && !!s.device!.bias_tee, on: s.tuning?.bias_tee === true },
    frequencyRanges: s.device?.frequency_ranges_hz ?? [],
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
