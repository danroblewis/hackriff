// Tuning nudges (T-409): shift the tuned centre off the DC/LO spike without losing the signal from
// the band.
//
// **Why this is not a convenience.** A front end mixes to baseband, so the centre of every window
// carries the tuner's own DC/LO leakage spike — measured in this repo more than once (T-317 flags
// `spur_reason` on it, T-382 put the host comb at −678.048 Hz relative to tuner DC, and T-394 leaves
// the fixture's tuner-DC box above 20 dB). A signal parked on the centre frequency is the one signal
// in the window that cannot be cleanly demodulated or decoded, and the centre frequency is exactly
// where a user naturally tunes. These buttons move the radio out from under it.
//
// **Three things this control has to get right, and none of them is the click handler.**
//
//  1. **It snaps.** Achievable centres are a grid, so `centre + fraction × span` is not generally
//     reachable and the actual shift differs from the advertised fraction by up to half a step.
//     `navigation.nudgePlan` does that arithmetic — T-341's `snapCenter`, not a second copy of it —
//     and the difference is printed rather than absorbed.
//  2. **It never clamps.** At the edge of the device's range a clamped nudge moves *less than the
//     button says*, which is a control lying quietly. A nudge that cannot be taken in full is
//     **disabled**, with the frequency it would have needed named in the tooltip. (T-392 met the
//     same problem and drew the same line: `containsCenter` is not `snapCenter`.)
//  3. **The largest fraction is the trap.** Half a span moves a centred signal to the *band edge* —
//     out of the LO spike and into the anti-alias roll-off, one hazard swapped for the other. A
//     quarter span lands it at the midpoint of the usable half-band, maximally far from both, which
//     is the same derivation `navigation.dcOffsetHz` already makes for T-418. The user asked for all
//     three fractions and gets all three: ¼ is marked as the derived ideal, ½ is marked as landing
//     in the skirt, and neither is decided for them.
//
// **It reaches the radio through the one gate.** A button press is an explicit, discrete user action
// — like the retune-offer button and T-392's region-select-on-release — so it *may* command the
// front end. It does so through `view.ts`'s typed `DeviceAction`, which is why no device route is
// named in this file (`ui/test/app-centre.test.ts` asserts exactly two files name one) and why
// T-340's "no pan and no wheel reaches a device route" is untouched: there is no gesture here at all.
//
// **One capture at a time.** Each press costs a settle gap, so the whole row is disabled while a
// nudge is in flight, and the press after it is computed from the centre this one asked for rather
// than from a control-state poll that has not caught up — two presses in a second must be two
// nudges, not the same nudge twice.
import * as ax from "../../axis";
import {
  NUDGE_FRACTIONS, NUDGE_IDEAL_FRACTION, nudgePlan, type CenterGrid, type NudgePlan,
} from "../../navigation";
import type { AppContext } from "../context";
import { h } from "../dom";
import { applyDeviceAction, mayRetune, retuneAction, NOT_LIVE_TEXT } from "./view";

/** Everything a nudge button's model is derived from. All of it is state already in hand — the
 * control-state poll's device block and the stream header's window — so nothing here measures
 * anything or asks the backend a question. */
export interface NudgeInput {
  /** Whether a retune may be attempted at all (`mayRetune`): a replay has no radio to move. */
  live: boolean;
  /** The centre to nudge from: the window in force, or the centre a nudge already in flight asked
   * for. Null when neither is known. */
  fromHz: number | null;
  /** The tuned span — the sample rate, not the zoom width. The fractions are of *this*. */
  spanHz: number | null;
  /** The achievable centre axis (`/api/control/state`'s `device`), or null when none is reported. */
  grid: CenterGrid | null;
  /** The front end a press would move, named on the button's face per T-343. */
  deviceId: string | null;
  /** True while a nudge is in flight: the radio is one shared resource and is mid-settle. */
  busy: boolean;
}

/** One button, fully decided. Rendering it adds no decisions. */
export interface NudgeButton {
  /** Signed fraction of the tuned span: negative moves the centre down in frequency. */
  fraction: number;
  label: string;
  ariaLabel: string;
  title: string;
  disabled: boolean;
  /** `"edge"` when a signal currently on DC would land at or past the window edge, in the
   * anti-alias roll-off — the ½-span trap, marked rather than hidden or removed. */
  hazard: "edge" | null;
  /** True for the fraction whose landing point is the derived ideal (`NUDGE_IDEAL_FRACTION`). */
  ideal: boolean;
  /** The centre the press asks for, or null when the press is refused. */
  centerHz: number | null;
}

/** ⅛ / ¼ / ½ as the button prints them — the fraction, not a rounded kHz that would change with
 * every retune. */
const FRACTION_GLYPH = new Map<number, string>([[1 / 8, "⅛"], [1 / 4, "¼"], [1 / 2, "½"]]);
const glyph = (f: number) => FRACTION_GLYPH.get(Math.abs(f)) ?? `${Math.abs(f)}`;
const WORDS = new Map<number, string>([[1 / 8, "an eighth"], [1 / 4, "a quarter"], [1 / 2, "a half"]]);
const words = (f: number) => WORDS.get(Math.abs(f)) ?? `${Math.abs(f)}`;

/** A percentage, for the two fractions-of-the-window the tooltip states. */
const pct = (x: number) => `${Math.round(x * 100)}%`;

/**
 * Whether a button's fraction is the derived ideal — decided from the **fraction**, which is exact,
 * not from the landing point, which the snap moves by `step/span` either way.
 *
 * That is not a shortcut: `ideal` is a property of the offer (a quarter of the span is the midpoint
 * of the usable half-band), and the snap is a hardware detail reported separately as `snapErrorHz`.
 * A mark that flickered on and off between retunes because the synthesiser rounded the other way
 * would be a worse control than no mark at all.
 */
const isIdeal = (fraction: number) => Math.abs(fraction) === NUDGE_IDEAL_FRACTION;

/** Resolution to print a nudged centre at: taken from the shift being described, so a 300 kHz nudge
 * and a 30 Hz snap error are each printed at a precision that shows them. Presentation only. */
const res = (hz: number) => Math.max(1, Math.abs(hz) / 100);

/** The static explanation the group carries — what these buttons are *for*, which is not guessable
 * from six arrows. */
export const NUDGE_HINT =
  "Shift the tuned centre by a fraction of the span, to move a signal off the tuner's DC/LO spike "
  + "without losing it from the window.";

/** Why a refused nudge is refused, in the user's terms — and, for the one that matters, why the
 * answer is a disabled button rather than a shorter move. */
export function nudgeRefusalText(p: Extract<NudgePlan, { ok: false }>, deviceId: string | null): string {
  switch (p.reason) {
    case "no_grid":
      return "This source reports no tunable range, so there is no achievable centre to nudge to.";
    case "no_center":
      return "The tuned centre is not known yet.";
    case "no_span":
      return "The tuned span is not known yet, so a fraction of it names no distance.";
    case "out_of_range": {
      const where = p.requestedHz === null ? "that centre" : `${ax.fmtMHz(p.requestedHz, 1e3)} MHz`;
      const on = deviceId ? ` ${deviceId}` : " this front end";
      // The deliberate half of T-392's choice, said out loud: the alternative is a button that moves
      // less than it claims, and a control that quietly under-delivers is worse than one that is off.
      return `Cannot be taken in full: ${where} is outside what${on} can tune. `
        + "The nudge is disabled rather than clamped — a clamped nudge would move less than the button says.";
    }
  }
}

/** What a takeable nudge will actually do: the move, where a centred signal lands, how much of the
 * window survives it, and — when the grid moved it — that the shift is not exactly the advertised
 * fraction. Every clause is a fact about this press, not a general caution. */
export function nudgeTitleText(
  p: Extract<NudgePlan, { ok: true }>, deviceId: string | null, ideal = false,
): string {
  const g = p.geometry;
  const dir = p.shiftHz >= 0 ? "up" : "down";
  const on = deviceId ? ` on ${deviceId}` : "";
  const to = p.onGrid
    ? `to ${ax.fmtMHz(p.centerHz, res(p.shiftHz))} MHz`
    // Rule 1 in navigation.ts: with no stated step, nothing may claim the radio sits exactly here.
    : `to about ${ax.fmtMHz(p.centerHz, res(p.shiftHz))} MHz (this source states no tuning step, so nothing is snapped)`;
  const parts = [
    `Moves the radio${on} ${dir} ${ax.fmtBandwidth(Math.abs(p.shiftHz))}, ${to}.`,
    `A signal on the DC spike now lands ${pct(g.landing)} of the way to the window edge, `
    + `and ${pct(g.overlap)} of the window is kept.`,
  ];
  if (g.intoSkirt) {
    // The third thing to get right, on the button that gets it wrong. Not removed — the user asked
    // for this fraction and it is the right stride for stepping along a band — but never silent.
    parts.push(
      "That is the band edge, where the anti-alias filter rolls off: this trades the DC spike for "
      + "the skirt. ¼ span lands a centred signal midway between the two, which is the only offset "
      + "here with a derivation behind it.",
    );
  } else if (ideal) {
    parts.push(
      "The derived ideal: the midpoint of the usable half-band, maximally far from both the LO "
      + "spike at DC and the anti-alias roll-off at the edge.",
    );
  }
  if (p.snapErrorHz !== 0) {
    parts.push(
      `Snapped to the tuning grid, so the move is ${ax.fmtBandwidth(Math.abs(p.snapErrorHz))} `
      + `${p.snapErrorHz * Math.sign(p.advertisedHz) > 0 ? "past" : "short of"} the advertised fraction.`,
    );
  }
  parts.push("A retune briefly interrupts capture; panning and zooming do not.");
  return parts.join(" ");
}

/**
 * The six buttons, left-moving first, each resolved against the achievable grid.
 *
 * Order is the frequency axis's own: the widest leftward nudge at the left end, the widest rightward
 * one at the right, so the row reads like the thing it moves.
 */
export function nudgeButtons(input: NudgeInput): NudgeButton[] {
  const signed = [
    ...[...NUDGE_FRACTIONS].reverse().map((f) => -f),
    ...NUDGE_FRACTIONS,
  ];
  return signed.map((fraction) => {
    const dir = fraction < 0 ? "down" : "up";
    const label = fraction < 0 ? `◂${glyph(fraction)}` : `${glyph(fraction)}▸`;
    const ariaLabel = `Nudge the tuned centre ${dir} by ${words(fraction)} of the span`;
    const ideal = isIdeal(fraction);
    if (!input.live) {
      return { fraction, label, ariaLabel, title: NOT_LIVE_TEXT, disabled: true, hazard: null, ideal, centerHz: null };
    }
    const p = nudgePlan(input.grid, input.fromHz, input.spanHz, fraction);
    if (!p.ok) {
      return {
        fraction, label, ariaLabel, title: nudgeRefusalText(p, input.deviceId), disabled: true,
        hazard: null, ideal, centerHz: null,
      };
    }
    return {
      fraction, label, ariaLabel,
      title: nudgeTitleText(p, input.deviceId, ideal),
      // Not a refusal: the radio is simply mid-settle, and the row comes back when it lands.
      disabled: input.busy,
      hazard: p.geometry.intoSkirt ? "edge" : null,
      ideal,
      centerHz: p.centerHz,
    };
  });
}

/**
 * Mounts the nudge row.
 *
 * `pending` is the centre the last press asked for. It is held until the observed centre *changes*
 * — not until it equals what was asked for, which would strand the row forever if the front end
 * landed somewhere else — so a second press inside the control-state poll's 2 s window nudges from
 * where the radio is going rather than from where it was.
 */
export function mountNudge(el: HTMLElement, ctx: AppContext) {
  const { store } = ctx;
  el.replaceChildren();
  let pending: number | null = null;
  let busy = false;

  /** The centre the radio has actually reported, header first (it lands before the next
   * control-state poll). */
  const observed = (s: ReturnType<typeof store.get>) => s.live.centerHz ?? s.device.centerHz;
  const input = (): NudgeInput => {
    const s = store.get();
    return {
      live: mayRetune(s.device),
      fromHz: pending ?? observed(s),
      // The tuned span, which is the sample rate — never `live.view`'s width, which is the zoom.
      spanHz: s.live.bandwidthHz ?? s.device.sampleRateHz,
      grid: s.device.centerGrid,
      deviceId: s.device.deviceId,
      busy,
    };
  };

  // The buttons are built once and updated in place. Replacing the row on every render would throw
  // away keyboard focus each time the radio answers — and the row is disabled and re-enabled around
  // every press, so that is precisely when a keyboard user is standing on one of them.
  const buttons = nudgeButtons(input()).map((m) => {
    const btn = h("button", { class: "nudge-btn", type: "button", "data-frac": String(m.fraction) }, m.label);
    // The one explicit user action here that reaches the front end, through T-343's single gate.
    // The plan is recomputed on press rather than captured at render: the tooltip is a preview of
    // what the press would do, and the press must act on where the radio is *now*.
    btn.addEventListener("click", () => { void press(m.fraction); });
    return btn;
  });

  const render = () => {
    const models = nudgeButtons(input());
    models.forEach((m, i) => {
      const btn = buttons[i];
      btn.title = m.title;
      btn.setAttribute("aria-label", m.ariaLabel);
      btn.disabled = m.disabled;
      if (m.hazard) btn.dataset.hazard = m.hazard; else delete btn.dataset.hazard;
      if (m.ideal) btn.dataset.ideal = ""; else delete btn.dataset.ideal;
    });
  };

  async function press(fraction: number) {
    if (busy) return;
    const m = nudgeButtons(input()).find((b) => b.fraction === fraction);
    if (!m || m.disabled || m.centerHz === null) return;
    busy = true;
    pending = m.centerHz;
    render();
    try {
      await applyDeviceAction(ctx, retuneAction(m.centerHz, "nudge"));
    } finally {
      busy = false;
      render();
    }
  }

  el.replaceChildren(h("span", { class: "nudge-label", title: NUDGE_HINT }, "Nudge"), ...buttons);
  el.title = NUDGE_HINT;
  // Any movement of the observed centre retires the pending one: the radio has answered, and its
  // answer outranks what was asked for.
  store.select(observed, () => { pending = null; render(); });
  store.select((s) => `${s.device.live}|${s.device.loaded}|${s.device.sampleRateHz}|${s.device.deviceId}`, render);
  store.select((s) => s.live.bandwidthHz, render);
  store.select((s) => s.device.centerGrid, render, { immediate: true });
}
