// Frequency entry, steps and shifts for the control panel (T-051). Pure: unit-tested in
// ui/test/controls.test.ts.

/** Multipliers for the unit letters an entry may carry (SDR++-style: `k`, `M`/`m`, `G`). */
const UNIT: Record<string, number> = { "": 1, k: 1e3, m: 1e6, g: 1e9 };

/**
 * A bare number (no unit) below this is read as MHz: no receiver this UI drives tunes below
 * 100 kHz, so `101.3` means 101.3 MHz, while `915000000` stays Hz.
 */
export const BARE_MHZ_BELOW = 1e5;

/**
 * Parses a frequency entry to Hz: `101.3M`, `433.92 MHz`, `2.4e6`, `915000000`, `7k`, `1.09 GHz`,
 * `100 Hz`, or `101.3` (bare, below [`BARE_MHZ_BELOW`]: MHz). A leading `+` or `-` makes it
 * relative to `currentHz` (`+25k`, `-1M`); relative bare numbers are Hz. Null when unparseable,
 * not finite, or not positive.
 */
export function parseFrequency(text: string, currentHz?: number): number | null {
  const m = /^\s*([+-])?\s*(\d+(?:\.\d*)?|\.\d+)(?:e([+-]?\d+))?\s*([kmg])?\s*(hz)?\s*$/i.exec(text);
  if (!m) return null;
  const [, sign, mant, exp, unit = "", hz] = m;
  let v = Number(mant) * (exp ? Math.pow(10, Number(exp)) : 1) * UNIT[unit.toLowerCase()];
  const bare = !unit && !hz && !exp;
  if (sign) {
    if (currentHz === undefined || !Number.isFinite(currentHz)) return null;
    v = currentHz + (sign === "-" ? -v : v);
  } else if (bare && v < BARE_MHZ_BELOW) v *= 1e6;
  v = Math.round(v * 1e3) / 1e3; // mHz: kills float noise from `433.92 * 1e6`
  return Number.isFinite(v) && v > 0 ? v : null;
}

/** A frequency with the largest unit that keeps it ≥ 1, trailing zeros trimmed (Hz resolution). */
export function formatFrequency(hz: number): string {
  if (!Number.isFinite(hz)) return "–";
  const a = Math.abs(hz);
  const [div, unit, dec] = a >= 1e9 ? [1e9, "GHz", 9] : a >= 1e6 ? [1e6, "MHz", 6] : a >= 1e3 ? [1e3, "kHz", 3] : [1, "Hz", 0];
  let s = (hz / div).toFixed(dec);
  if (dec) s = s.replace(/0+$/, "").replace(/\.$/, "");
  return `${s} ${unit}`;
}

/** A step choice: a fixed size (snaps to its grid) or a fraction of the displayed span (no snap). */
export type Step = { hz: number } | { spanFrac: number };

export const STEPS: readonly { label: string; step: Step }[] = [
  { label: "1 kHz", step: { hz: 1e3 } },
  { label: "5 kHz", step: { hz: 5e3 } },
  { label: "6.25 kHz", step: { hz: 6.25e3 } },
  { label: "10 kHz", step: { hz: 10e3 } },
  { label: "12.5 kHz", step: { hz: 12.5e3 } },
  { label: "25 kHz", step: { hz: 25e3 } },
  { label: "100 kHz", step: { hz: 100e3 } },
  { label: "200 kHz", step: { hz: 200e3 } },
  { label: "1 MHz", step: { hz: 1e6 } },
  { label: "5 MHz", step: { hz: 5e6 } },
  { label: "½ span", step: { spanFrac: 0.5 } },
  { label: "span", step: { spanFrac: 1 } },
];

/** Default: half a span, so repeated shifts walk a band with overlap (a survey, not a channel list). */
export const DEFAULT_STEP = STEPS.findIndex((s) => "spanFrac" in s.step && s.step.spanFrac === 0.5);

/** The step size in Hz for a span. */
export const stepHz = (s: Step, spanHz: number) => ("hz" in s ? s.hz : s.spanFrac * spanHz);

/** Clamps `hz` into the nearest of the device's frequency ranges (`[min, max]` pairs); unchanged without ranges. */
export function clampToRanges(hz: number, ranges: readonly (readonly [number, number])[] | undefined): number {
  if (!ranges?.length) return hz;
  let best = hz, bestD = Infinity;
  for (const [lo, hi] of ranges) {
    const c = Math.min(hi, Math.max(lo, hz)), d = Math.abs(c - hz);
    if (d < bestD) { best = c; bestD = d; }
  }
  return best;
}

/**
 * The centre after one shift (`dir` +1 up, −1 down). A fixed step lands on its grid: from 101.33 MHz
 * with 100 kHz, up is 101.4 and down is 101.3 (like SDR++ snapping). A span step moves by that
 * much. The result is clamped into the device's ranges.
 */
export function shiftCenter(centerHz: number, s: Step, spanHz: number, dir: 1 | -1,
  ranges?: readonly (readonly [number, number])[]): number {
  let next: number;
  if ("hz" in s) {
    const k = centerHz / s.hz, eps = 1e-6;
    next = (dir > 0 ? Math.floor(k + eps) + 1 : Math.ceil(k - eps) - 1) * s.hz;
  } else next = centerHz + dir * stepHz(s, spanHz);
  return clampToRanges(Math.round(next * 1e3) / 1e3, ranges);
}
