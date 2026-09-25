// **The two contrast buttons, as one rule** (T-528).
//
// The surface has three ways of deciding its one display range (`./surface.ts`'s [[RangeMode]]) and
// two buttons to choose between them. Two booleans in each host would be two chances to disagree
// about what `Auto-contrast: on` plus `Viewport scale: off` means — and there are two hosts, the
// app's Explore centre and the `/surface` preview page, which is exactly the shape of every
// duplicated-arithmetic defect this milestone has closed (T-412's two wheel handlers, T-397's two
// ramps). So the mode is the single piece of state, each press is a pure function of it, and the
// labels are derived from it rather than tracked beside it.
//
// **Persistence.** Per-viewer display preference, in `localStorage`, every access wrapped — the
// same discipline as `./shadow-gain.ts` and `readShowSignals`. It is never sent anywhere and the
// backend has no opinion about it. An unreadable, absent or unrecognised value is
// [[DEFAULT_RANGE_MODE]], which is `anchored`: **today's behaviour is the default**, because it is
// the only one of the three whose colours mean the same thing at every zoom, and a new viewer
// should not have to know that the picture is re-scaling under them.

import type { RangeMode } from "./surface";

export const RANGE_MODE_KEY = "hk-surface-range-mode";

/** Anchored — what the surface did before T-528, and what a viewer who has pressed nothing gets. */
export const DEFAULT_RANGE_MODE: RangeMode = "anchored";

const MODES: readonly RangeMode[] = ["anchored", "auto", "viewport"];

/** Read the persisted mode. Absent, unreadable or unrecognised all mean [[DEFAULT_RANGE_MODE]]. */
export function loadRangeMode(): RangeMode {
  try {
    const v = localStorage.getItem(RANGE_MODE_KEY);
    return MODES.includes(v as RangeMode) ? (v as RangeMode) : DEFAULT_RANGE_MODE;
  } catch {
    return DEFAULT_RANGE_MODE;
  }
}

export function saveRangeMode(mode: RangeMode): void {
  try { localStorage.setItem(RANGE_MODE_KEY, mode); } catch { /* storage unavailable */ }
}

/**
 * **`Auto-contrast`'s press.** Off is always `anchored`; on returns to the cheap tile-range tracker.
 *
 * Pressing it while the viewport mode is in force turns tracking *off* — which is what the label
 * says it does, and the alternative (a press that demotes one tracking mode to another) would make
 * the button's own state unreadable.
 */
export function pressAutoContrast(mode: RangeMode): RangeMode {
  return mode === "anchored" ? "auto" : "anchored";
}

/**
 * **`Viewport scale`'s press.** On is the viewport-dynamic measurement; off leaves tracking on and
 * falls back to the tile ranges.
 *
 * Turning it on from `anchored` turns auto-contrast on with it, because a viewport-measured range
 * *is* a tracking range — offering it as an independent switch that silently does nothing until a
 * second button is pressed would be a control that lies about being a control.
 */
export function pressViewportScale(mode: RangeMode): RangeMode {
  return mode === "viewport" ? "auto" : "viewport";
}

/** One button's whole presentation: what it reads, what it claims, and whether it is lit. */
export interface ContrastButton {
  readonly label: string;
  readonly title: string;
  readonly pressed: boolean;
}

/** `Auto-contrast`'s label and hover text for `mode`. */
export function autoContrastButton(mode: RangeMode): ContrastButton {
  const pressed = mode !== "anchored";
  return {
    label: `Auto-contrast: ${pressed ? "on" : "off"}`,
    pressed,
    title: pressed
      ? "The display range tracks what is on screen: nothing clips, but the same signal changes colour as you navigate. Press to go back to the anchored range."
      : "The display range is anchored to the region, so the same measured dB is the same colour at every zoom — at the cost of clipping outside it. Press to track what is on screen instead.",
  };
}

/** `Viewport scale`'s label and hover text for `mode`. */
export function viewportScaleButton(mode: RangeMode): ContrastButton {
  const pressed = mode === "viewport";
  return {
    label: `Viewport scale: ${pressed ? "on" : "off"}`,
    pressed,
    title: pressed
      ? "The range is measured from the observed cells inside this view — last-known (shadow) cells excluded, since they measure an earlier time. A quiet band fills the ramp, and panning re-measures it. Press to scale from whole tiles instead."
      : "Press to measure the range from the observed cells inside the view rather than from whole tiles, so a quiet band spreads across the ramp instead of sitting at the bottom of it. Turns auto-contrast on.",
  };
}

/**
 * **The same three modes as one radio group** (T-882): the layers menu's "Colour scale" axis, which
 * replaced the toolbar's two buttons. One row per mode, derived from the mode — never tracked beside
 * it — so the menu and the surface cannot disagree about which scale is in force. The hints are the
 * buttons' trades, shortened; the full sentence is the range label beside the group.
 */
export function scaleRows(mode: RangeMode): { id: RangeMode; label: string; hint: string; on: boolean }[] {
  return [
    { id: "anchored", label: "Anchored", hint: "same dB, same colour at every zoom", on: mode === "anchored" },
    { id: "auto", label: "Auto-contrast", hint: "tracks the tiles on screen", on: mode === "auto" },
    { id: "viewport", label: "Viewport scale", hint: "observed cells in view", on: mode === "viewport" },
  ];
}

/** Parse a scale row's id back to a mode; anything else is `null` (the press is ignored). */
export function scaleMode(id: string): RangeMode | null {
  return (MODES as readonly string[]).includes(id) ? (id as RangeMode) : null;
}
