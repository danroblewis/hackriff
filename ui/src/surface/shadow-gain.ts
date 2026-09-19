// **User-adjustable shadow brightness** (T-526, client-only). `cellrule.ts`'s `SHADOW_MARK.gain`
// (0.25) is still the default and still the number the shadow's hard-ceiling guarantee is written
// against at its top end (0.7, this module's `SHADOW_GAIN_MAX`) — `surface-honesty.test.ts` asserts
// the live ramp clears any gain up to that max from about a third of the way up, so widening how
// dark a viewer may render a shadow can never turn one into something read as live. Nothing here
// touches the backend: it is a per-viewer display preference, threaded into the shader as a uniform
// (`Surface.shadowGain`, set from `uShadowGain`) and re-rendered instantly — no refetch, no change
// to what was recorded.

/** Where the choice is remembered. A per-viewer convenience (browser storage), not shared state:
 * see CLAUDE.md's rule on what belongs in `localStorage` versus a runtime capability. */
export const SHADOW_GAIN_KEY = "hk-mui-shadow-gain";

/** Must match `SHADOW_MARK.gain` in `cellrule.ts` — the shadow's shipped default. */
export const SHADOW_GAIN_DEFAULT = 0.25;
export const SHADOW_GAIN_MIN = 0.05;
export const SHADOW_GAIN_MAX = 0.7;

/** Multiplicative step per wheel notch: a scroll ramps the brightness rather than jumping it. */
const STEP = 1.08;

export function clampShadowGain(v: number): number {
  if (!Number.isFinite(v)) return SHADOW_GAIN_DEFAULT;
  return Math.min(SHADOW_GAIN_MAX, Math.max(SHADOW_GAIN_MIN, v));
}

/** `notches` is the wheel's own signed count for this gesture; a positive notch brightens. */
export function stepShadowGain(current: number, notches: number): number {
  return clampShadowGain(current * Math.pow(STEP, notches));
}

/** The remembered gain, or the default if nothing is stored, storage is unavailable (private
 * browsing, blocked site data), or the stored value doesn't parse. A broken localStorage must never
 * take the shadow mark's readability down with it. */
export function loadShadowGain(): number {
  try {
    const raw = localStorage.getItem(SHADOW_GAIN_KEY);
    if (raw === null) return SHADOW_GAIN_DEFAULT;
    const v = Number(raw);
    return Number.isFinite(v) ? clampShadowGain(v) : SHADOW_GAIN_DEFAULT;
  } catch {
    return SHADOW_GAIN_DEFAULT;
  }
}

export function saveShadowGain(v: number): void {
  try {
    localStorage.setItem(SHADOW_GAIN_KEY, String(clampShadowGain(v)));
  } catch {
    // Per-viewer convenience only; nothing downstream depends on the write succeeding.
  }
}

/** The one implementation of "a Ctrl+Shift wheel notch changes the shadow gain" (T-397's rule): both
 * hosts that mount `attachSurfaceInput` (`preview-main.ts`, `app/centre/surface.ts`) pass this as
 * `onShadowGain`, so there is no second place the step/clamp/persist arithmetic could drift. */
export function shadowGainWheelHandler(
  surface: { shadowGain: number; setShadowGain: (g: number) => void },
): (notches: number) => void {
  return (notches: number) => {
    const g = stepShadowGain(surface.shadowGain, notches);
    surface.setShadowGain(g);
    saveShadowGain(g);
  };
}
