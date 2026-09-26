// **Retune mode** (T-1028): the one state in which a settled pan/zoom commands the radio.
//
// ## What this changes, stated plainly, because it amends an invariant
//
// The project's navigation rule has always been *"a pan or wheel never commands the radio"* (root
// `CLAUDE.md`, docs/23 §4/§10.4, ADR-0023), enforced by spy-client tests that drive the whole
// gesture vocabulary and assert an **empty call list**. The user amended it on 2026-09-25:
//
// > *"I think the 'retune' functionality would be cool as a mode, like they can turn on 'retune'
// > mode and whenever they zoom or pan it retunes to that. For areas that are too large and can't
// > be tuned, use the largest possible size instead of denying them. This mode gets turned off.
// > Maybe it's a keyboard thing, hold down a certain key while panning to retune."*
//
// So the rule is now: **by default, unchanged** — pan and wheel reach nothing, and the empty-call-list
// control still runs and still passes. **In retune mode, which is explicit, visible and off by
// default, the view's frequency window IS the tune request.** The safety model is not weakened by
// removing the line between view arithmetic and a device command; it is kept by making the *mode*
// the discrete, explicit act that T-407/T-444 required of the press. The user turns it on, sees it
// on, and can hold a key for exactly one gesture instead.
//
// ## The five properties this file exists to hold
//
//  1. **Off by default, and off means the old rule verbatim.** Nothing here is reachable with the
//     mode off: [[RetuneModeController.moved]] and [[RetuneModeController.settled]] both return
//     immediately, so a host that wires the gestures in unconditionally still produces no call.
//  2. **A gesture commands the radio once, when it SETTLES** — the pointer released (or the pinch
//     ended), or ~150 ms of stillness for a wheel, which has no release. Never per pointer move: a
//     drag across 6 GHz would otherwise be a thousand retunes, and the front end is one shared
//     resource with a settle gap.
//  3. **The latest settled view wins, and a retune in flight is never cancelled.** A request that
//     is already out stays out — cancelling a device action mid-flight is exactly the race T-343's
//     one-capture-at-a-time gate exists to avoid — and the next one waits the same settle interval
//     after it resolves. Three quick pans are therefore one retune, to the last view.
//  4. **Too wide is not an error here.** [[fitToLiveWindow]] narrows a view wider than one capture
//     window to the largest achievable window centred on it. The pane goes on showing the wide view,
//     with the coverage fog showing which part of it the radio actually took — the honesty tiers are
//     untouched, and nothing claims detail outside the tuned window. The refusal that remains is the
//     one the user kept: a view with no overlap at all with the tunable range (or a replay's extent).
//  5. **Frequency only.** Nothing here touches time, the pane's follow/frozen state, the ring or
//     detection. A frozen pane in retune mode retunes and **stays frozen** — its window is a fact
//     about the view, and a retune changes only what is captured from here on. That is why the
//     [[PaneRetuneBlock]] `"past"` refusal of the press-a-button control is deliberately NOT applied
//     in the mode: the button's refusal says *"there is nothing here for a retune to sharpen"*, which
//     is advice about a control the user did not ask for; the mode's user asked for this one and is
//     owed the radio moving, not a silent no.
//
// ## Where the arithmetic is, and where it is not
//
// This file computes no RF fact of its own. `fitToLiveWindow` (navigation.ts) narrows the region;
// `retunePlan` (navigation.ts) turns a region into a capture configuration — the SAME planner the
// pane's Retune button and the width presets call, so the mode cannot disagree with the button about
// what the front end can do. `applyDeviceAction` (app/centre/view.ts) is the one gated path to the
// device, unchanged: one capture at a time, the settle gap, `device_id` recorded by the backend,
// `device_busy` surfaced rather than raced. This file names no route.

import { fitToLiveWindow, retunePlan, type FrequencyGrid, type RetunePlan, type RetuneRefusal } from "../navigation";
import { applyDeviceAction, retuneAction, type DeviceAction } from "../app/centre/view";
import type { AppContext } from "../app/context";
import { boxOf, timeExtentOf, type PaneState } from "./panes";

/**
 * How long a view must stand still before a wheel/pinch gesture counts as settled, ms.
 *
 * A drag and a pinch have a release the browser reports, and that release is the settle — this
 * timer is for the gesture that has none. It is also what makes "three quick pans are one retune"
 * true across the boundary between two strokes: a second stroke that starts before the first one's
 * retune has gone out replaces the target rather than adding a request.
 *
 * The number is the user's (~150 ms). It is short enough that the radio follows the view as fast as
 * a person can tell, and long enough that the notches of one wheel gesture are one request.
 */
export const RETUNE_SETTLE_MS = 150;

/**
 * What a settled view asks the front end for.
 *
 * `viewLoHz`/`viewHiHz` are the pane's own window — what the user is looking at — and `loHz`/`hiHz`
 * are the region the plan was made for, which is the same thing unless the view was wider than one
 * capture window ([[narrowed]]). Both are carried because the difference is precisely what the
 * status line has to say out loud: the radio took *this much* of what you are looking at.
 */
export interface RetuneModeTarget {
  readonly paneId: string;
  /** The pane's coverage selector — whose window this is about (`"any"` = the union). */
  readonly device: string;
  /** The region planned for: the view, or the largest achievable window centred on it. */
  readonly loHz: number;
  readonly hiHz: number;
  /** The pane's own frequency window, always — never narrowed. */
  readonly viewLoHz: number;
  readonly viewHiHz: number;
  /** True when the view was wider than one capture window, so the plan covers part of it. */
  readonly narrowed: boolean;
  /** True when the pane is frozen behind the growing edge. Stated, never a refusal here (see the
   * header's property 5): a retune in the mode is frequency-only and leaves the freeze alone. */
  readonly frozen: boolean;
  /** The capture configuration, or the reason none can capture even the narrowed region. */
  readonly plan: RetunePlan;
}

/**
 * What a settled view in retune mode asks for — **always a target, never null**, on the same
 * "stated and disabled, never hidden" rule `retune.ts` already lives by: a view the front end
 * cannot reach produces a target carrying the refusal, so the status line can say why the radio did
 * not move instead of leaving the user to infer it from silence.
 */
export function retuneModeTarget(
  pane: PaneState, grid: FrequencyGrid | null, edgeNs: number, edgeGraceNs = 0,
): RetuneModeTarget {
  const box = boxOf(pane, edgeNs);
  const t = timeExtentOf(pane.time, edgeNs);
  // The view is narrowed FIRST and planned second, so there is exactly one planner and the mode
  // cannot invent a configuration the button would refuse. A view no wider than one window comes
  // back unchanged, which is why the ordinary case has no special path here.
  const fit = fitToLiveWindow(grid, box.f0Hz, box.f1Hz);
  const loHz = fit?.loHz ?? box.f0Hz, hiHz = fit?.hiHz ?? box.f1Hz;
  return {
    paneId: pane.id,
    device: pane.device,
    loHz,
    hiHz,
    viewLoHz: box.f0Hz,
    viewHiHz: box.f1Hz,
    narrowed: hiHz - loHz < box.f1Hz - box.f0Hz,
    frozen: t.t1Ns + edgeGraceNs < edgeNs,
    plan: retunePlan(grid, loHz, hiHz),
  };
}

/** Whether a target can be sent. A refusal is stated, never clamped into something takeable. */
export const retuneModeAcceptable = (t: RetuneModeTarget | null): boolean => !!t && t.plan.ok;

/** The reason no capture configuration reaches this view at all — the refusals the mode keeps. */
function refusalText(reason: RetuneRefusal): string {
  switch (reason) {
    case "center_out_of_range":
      return "Retune mode: this viewport is outside the front end's tunable range, so no retune can reach it.";
    case "span_too_wide":
      return "Retune mode: no capture window covers any of this viewport.";
    default:
      return "Retune mode: the front end has not reported a tunable range, so nothing may be claimed achievable.";
  }
}

/**
 * The one line the pane shows while the mode is acting on it — what the radio was asked for, or why
 * it was not asked for anything.
 *
 * It states the **narrowing** wherever one happened, because a user who zoomed out to 40 MHz and got
 * a 20 MHz capture must be able to read that off the screen rather than deduce it from the fog.
 */
export function retuneModeLabel(t: RetuneModeTarget): string {
  if (!t.plan.ok) return refusalText(t.plan.reason);
  const mhz = (t.plan.centerHz / 1e6).toFixed(4);
  const msps = (t.plan.spanHz / 1e6).toFixed(3);
  const view = ((t.viewHiHz - t.viewLoHz) / 1e6).toFixed(3);
  const head = `Retune mode: tuning ${mhz} MHz at ${msps} MHz span`;
  const narrowed = t.narrowed
    ? ` — the widest capture this front end has, centred on the ${view} MHz you are viewing; the rest of the view stays as it was last observed`
    : "";
  const frozen = t.frozen ? " This viewport stays frozen where it is: a retune changes only what is captured from now on." : "";
  return `${head}${narrowed}.${frozen}`;
}

/**
 * The typed device action a target becomes, or `null` for one that cannot be sent.
 *
 * `source: "retune-mode"` is its own variant in the audit trail — deliberately distinct from
 * `"pane-offer"` and `"pane-width"`, which are button presses — so that after the fact it is
 * possible to tell a retune the user *pressed* from one their pan asked for. That distinction is the
 * amended invariant made legible in the log.
 *
 * `want` is null: the view is the request, not something to restore. Nothing moved the pane, so
 * there is no pre-retune window to put back, and claiming one would fight the pane the user is
 * still holding.
 */
export function retuneModeAction(t: RetuneModeTarget | null): DeviceAction | null {
  if (!t || !retuneModeAcceptable(t) || !t.plan.ok) return null;
  return retuneAction(t.plan.centerHz, "retune-mode", null, t.plan.spanHz);
}

/** What [[commitRetuneMode]] needs from the surface around it. */
export interface RetuneModeSite {
  /** The target for this pane **now** — re-derived at the instant of the commit, because in this
   * mode the latest settled view is what was asked for. (This is the opposite of `acceptPaneRetune`'s
   * guard, and deliberately: there, a label was painted and the press must honour it; here, no label
   * was consented to — the *view* was, and the view is read fresh.) */
  targetNow(paneId: string): RetuneModeTarget | null;
  /** T-437 §5.2: the growing edge's tiles describe the tuning that has just ended. */
  invalidateEdge(): number;
}

export type RetuneModeOutcome =
  | { ok: true; action: DeviceAction; target: RetuneModeTarget; invalidated: number }
  | { ok: false; reason: "no_target" | "not_acceptable" | "refused"; target: RetuneModeTarget | null };

/**
 * Send one settled view to the front end, through the one gate.
 *
 * The order is `retune.ts`'s, minus the moved-target guard and for the reason given on
 * [[RetuneModeSite.targetNow]]: derive from live pane state, go through `applyDeviceAction`
 * (which snaps the centre again, posts one window, and surfaces `device_busy` rather than racing),
 * then drop the growing edge's tiles — but **only after the front end took it**, since a refused
 * retune left the tuning exactly where those tiles described.
 */
export async function commitRetuneMode(
  ctx: AppContext, site: RetuneModeSite, paneId: string,
): Promise<RetuneModeOutcome> {
  const target = site.targetNow(paneId);
  if (!target) return { ok: false, reason: "no_target", target: null };
  const action = retuneModeAction(target);
  if (!action) return { ok: false, reason: "not_acceptable", target };
  const ok = await applyDeviceAction(ctx, action);
  if (!ok) return { ok: false, reason: "refused", target };
  return { ok: true, action, target, invalidated: site.invalidateEdge() };
}

// ---------------------------------------------------------------------------
// The mode itself: a toggle, a held key, and the settle
// ---------------------------------------------------------------------------

/** A keyboard event, as much of one as the intent needs. Kept structural so the rule is testable
 * without a DOM, exactly like `active-pane.ts`'s `PaneKeyEvent`. */
export interface RetuneKeyEvent {
  key: string;
  repeat?: boolean;
  ctrlKey?: boolean;
  metaKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  defaultPrevented?: boolean;
  target?: unknown;
}

/**
 * The mode's key: **`R`**, held for one gesture or tapped to latch.
 *
 * Which key, and why not a modifier: T-456 already spends `Shift` (frequency-only zoom, and the
 * region stroke), `Alt` (time-only zoom) and `Ctrl`/`Cmd` (uniform zoom and the browser's own page
 * zoom), and `Ctrl+Shift` is T-526's shadow brightness. A bare letter collides with none of them,
 * and it is the only spare form that can be *held* — which the user asked for — while still being
 * tappable. With Ctrl, Cmd or Alt held the key belongs to the browser or the OS, and a key going
 * into a text field is typing: both refuse, which is `paneKeyIntent`'s rule and is why this returns
 * false rather than taking those events.
 *
 * `repeat` is not refused on the way down (a held key auto-repeats and the mode is still held) but
 * it is never a second request either — [[RetuneModeController.keyDown]] is idempotent.
 */
export function isRetuneKey(e: RetuneKeyEvent, isTyping: (t: unknown) => boolean): boolean {
  if (e.defaultPrevented || e.ctrlKey || e.metaKey || e.altKey) return false;
  if (isTyping(e.target)) return false;
  return e.key === "r" || e.key === "R";
}

/** The injected timer, so the settle is testable without a clock (`IdleFade`'s shape). */
export interface RetuneTimers {
  set(fn: () => void, ms: number): unknown;
  clear(t: unknown): void;
}

const REAL_TIMERS: RetuneTimers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (t) => clearTimeout(t as ReturnType<typeof setTimeout>),
};

export interface RetuneModeOptions {
  /** Send one settled view. Resolves when the front end has answered (taken or refused) — the
   * controller holds the next request until it does, which is property 3. */
  commit(paneId: string): Promise<unknown>;
  /** The mode turned on/off, a retune became pending, or one landed: re-paint the chip and the
   * pane's status line. Called on change only. */
  onChange?(): void;
  settleMs?: number;
  timers?: RetuneTimers;
}

/**
 * The mode's whole state machine: sticky on/off, the held key, the settle timer, and the
 * one-in-flight rule.
 *
 * It is deliberately **not** a store slice. Retune mode is an input mode — the same kind of state as
 * `measureMode`, which the surface host also keeps locally and `input.ts` reads live at the press —
 * and putting it in the app store would make every poll-driven re-render a place it could be read
 * stale, which for a state that decides whether a gesture moves a radio is the wrong trade.
 */
export class RetuneModeController {
  private sticky = false;
  private held = false;
  /** A gesture happened while the key was down, so the release is the END of a momentary mode and
   * not a tap. Without it, hold-drag-release would also toggle the sticky mode on. */
  private usedHeld = false;
  private timer: unknown = null;
  /** The pane whose settled view is waiting for the settle interval to elapse. */
  private waiting: string | null = null;
  /** The pane whose retune is out at the front end, or null. Never cancelled (property 3). */
  private flying: string | null = null;
  /** The latest settled pane while one was in flight — the request that goes out next. */
  private queued: string | null = null;
  private readonly settleMs: number;
  private readonly timers: RetuneTimers;
  /** The last state the host was told about, so a pointermove stream that re-arms the timer sixty
   * times a second does not re-render the chip sixty times. Everything the chip and the status line
   * read is in this key; nothing else is a change worth announcing. */
  private announced = "";

  constructor(private readonly opts: RetuneModeOptions) {
    this.settleMs = opts.settleMs ?? RETUNE_SETTLE_MS;
    this.timers = opts.timers ?? REAL_TIMERS;
    this.announced = this.key();
  }

  private key(): string {
    return `${this.sticky}|${this.held}|${this.flying ?? ""}|${this.waiting ?? ""}|${this.queued ?? ""}`;
  }

  /** Tell the host, on change only. */
  private announce(): void {
    const k = this.key();
    if (k === this.announced) return;
    this.announced = k;
    this.opts.onChange?.();
  }

  /** Is the mode on right now — latched, or the key held? */
  get on(): boolean { return this.sticky || this.held; }
  /** Is the LATCHED mode on? (The chip's pressed state; a held key is momentary and shows as active
   * without latching the chip.) */
  get latched(): boolean { return this.sticky; }
  get isHeld(): boolean { return this.held; }
  /** The pane a retune is pending or in flight for — what the status line is about, or null. */
  get pendingPane(): string | null { return this.flying ?? this.waiting ?? this.queued; }
  /** Is a retune out at the front end right now? */
  get inFlight(): boolean { return this.flying !== null; }

  /** The chip's press, and the `R` tap. */
  toggle(): void { this.setSticky(!this.sticky); }

  setSticky(on: boolean): void {
    if (this.sticky === on) return;
    this.sticky = on;
    if (!this.on) this.abandon();
    this.announce();
  }

  /** `R` went down: momentary mode for as long as it is held. Idempotent under auto-repeat. */
  keyDown(): void {
    if (this.held) return;
    this.held = true;
    this.usedHeld = false;
    this.announce();
  }

  /**
   * `R` came up. A hold that was *used* (a gesture moved a view while it was down) simply ends —
   * releasing restores mode-off behaviour, which is the user's "hold a key while panning". A hold
   * that was not used was a **tap**, and a tap toggles the sticky mode.
   *
   * Told apart by what happened, not by how long it took: a duration threshold on a keystroke is the
   * same class of rule as the 6 px drag threshold T-407 found two defects in, and this repo has a
   * standing preference for reading the events rather than the clock.
   */
  keyUp(): void {
    if (!this.held) return;
    const used = this.usedHeld;
    this.held = false;
    this.usedHeld = false;
    if (!used) { this.sticky = !this.sticky; }
    // Whatever the release meant, a settle still waiting from the momentary mode is abandoned when
    // the mode is now off: the user let go, and letting go must not command the radio a moment later.
    if (!this.on) this.abandon();
    this.announce();
  }

  /**
   * **The key is not held any more, and we never saw it come up** — the window lost focus with `R`
   * down (alt-tab, a dialog, a platform shortcut), so the `keyup` went somewhere else.
   *
   * Deliberately NOT [[keyUp]]: a release we did not observe is not a tap, and treating it as one
   * would *latch* the mode on a window switch — a mode that tunes the radio, turned on by leaving.
   * So this drops the hold and nothing else, and abandons a settle that was still waiting: the same
   * rule as a real release, minus the meaning a real release carries. (Found by review of the first
   * cut, which could leave `held` stuck true until `R` was pressed again.)
   */
  releaseHeld(): void {
    if (!this.held) return;
    this.held = false;
    this.usedHeld = false;
    if (!this.on) this.abandon();
    this.announce();
  }

  /**
   * A gesture moved a pane's view. With the mode off this is the whole of the work: nothing is
   * recorded and nothing is scheduled, so the default rule holds byte for byte.
   */
  moved(paneId: string): void {
    if (!this.on) return;
    if (this.held) this.usedHeld = true;
    this.arm(paneId);
  }

  /**
   * The gesture ENDED (pointer release, pinch end): that is the settle, and there is nothing to wait
   * for. A gesture with no release — a wheel — never calls this and is settled by the timer instead.
   */
  settled(paneId: string): void {
    if (!this.on) return;
    if (this.held) this.usedHeld = true;
    this.disarm();
    this.fire(paneId);
  }

  /** Drop any pending work (a mode turned off, a mount torn down). Never cancels a request already
   * at the front end: that one is out, and the backend owns it now. */
  dispose(): void { this.abandon(); }

  private abandon(): void {
    this.disarm();
    this.queued = null;
  }

  private disarm(): void {
    if (this.timer !== null) this.timers.clear(this.timer);
    this.timer = null;
    this.waiting = null;
  }

  /** (Re)start the stillness timer for `paneId`. A later move replaces the target and the wait. */
  private arm(paneId: string): void {
    this.disarm();
    this.waiting = paneId;
    this.timer = this.timers.set(() => {
      this.timer = null;
      this.waiting = null;
      this.fire(paneId);
    }, this.settleMs);
    this.announce();
  }

  /** Send, or queue behind the one in flight. The mode may have been switched off between the
   * settle and here (a key released during the wait), and then nothing goes out. */
  private fire(paneId: string): void {
    if (!this.on) return;
    if (this.flying !== null) { this.queued = paneId; this.announce(); return; }
    this.flying = paneId;
    this.announce();
    void Promise.resolve(this.opts.commit(paneId)).catch(() => {}).then(() => {
      this.flying = null;
      const next = this.queued;
      this.queued = null;
      this.announce();
      // The next request waits the same settle interval the first one did, rather than going out the
      // instant the front end answers: the device has its own settle gap, and a queued view is by
      // definition one the user has already stopped moving.
      if (next !== null && this.on) this.arm(next);
    });
  }
}
