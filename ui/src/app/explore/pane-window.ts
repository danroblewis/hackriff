// T-1002 (MMAP split view): **one pane's (time × frequency) window, as a query.**
//
// The inventory is time-scoped to the view (CLAUDE.md's signal model, ADR-0017 §2.1), and since
// MCANVAS "the view" is a *pane* — panes are independent in `surface/panes.ts`, each with its own
// centre and span in both axes. Before this ticket the Candidate/Confirmed queries were built from
// ONE window (`state.live.view` + `state.time`, the mirror of whichever pane was last touched), and
// every pane drew the same answer. So freezing pane 1 on a past signal re-scoped pane 2's live
// boxes to pane 1's past window — the split view's whole point, undone by a hidden shared window.
//
// The arithmetic here is exactly what `inventory.ts` did with the mirrored state, with the window
// named by the caller instead of read out of one global: a spec in, a `Filters` out. It is pure
// presentation arithmetic over already-known view state (the thin-client rule) — it builds no URL,
// makes no request and knows nothing about a device.
import type { Filters } from "../../inventory";

/** The UI's one time window, per pane: `[t0, t1]` on the capture clock. */
export interface ViewWindow { t0: number; t1: number }

/**
 * One pane's window, as the inventory queries need it: the frequency span it shows, whether it
 * follows the live edge, the instant it is frozen at when it does not, and the time span on screen.
 *
 * `id` is the pane's own id and `n` its 1-based position in layout order — the number the chrome
 * names it by (`centre/active-pane.ts`), carried here so a list can say *which* pane it is showing
 * without a second lookup. The empty id `""` is the **unpaned** window: the one query built from
 * the app's mirrored state before any pane registry exists (tests, and the moment before the
 * surface's first frame). It is not a pane and is never named.
 */
export interface PaneWindowSpec {
  id: string;
  n: number;
  /** The frequency span on screen, or `null` when no view has reported one yet. */
  loHz: number | null;
  hiHz: number | null;
  live: boolean;
  /** The instant the frozen pane's window ENDS at, on the capture clock; `null` while following. */
  tS: number | null;
  spanS: number;
}

/**
 * The frequency filters both of a pane's lists share — its own tuned/zoomed span. Deliberately
 * carries no time: the window belongs to the Candidate query alone (see [[paneCandidateFilters]]).
 *
 * `relations: "all"` is T-587's rule, unchanged: an artefact the backend has already explained must
 * reach the screen labelled rather than vanish behind the `shown` default.
 */
export function paneFilters(spec: PaneWindowSpec): Filters {
  const f: Filters = { relations: "all" };
  if (spec.loHz !== null && spec.hiHz !== null) { f.fLoHz = spec.loHz; f.fHiHz = spec.hiHz; }
  return f;
}

/**
 * The `[t0, t1]` **this pane's** Candidate list is scoped to — the window that pane is showing, and
 * nothing else.
 *
 * A following pane's window ends at `edge`, the capture clock's live edge; a frozen one's ends at
 * the instant it was frozen at, which is a property of the pane and not of the clock. `null` is
 * **unknown**: a following pane before any live edge has been reported (T-379). The caller must
 * then say so rather than invent a window — a fabricated window renders as emptiness that looks
 * exactly like a quiet band.
 */
export function paneWindow(spec: PaneWindowSpec, edge: number | null): ViewWindow | null {
  const span = spec.spanS > 0 ? spec.spanS : 0;
  if (span <= 0) return null;
  if (!spec.live && spec.tS !== null) return { t0: spec.tS - span, t1: spec.tS };
  return edge === null ? null : { t0: edge - span, t1: edge };
}

/** This pane's Candidate query: its frequency span plus its own `[t0, t1]`. */
export function paneCandidateFilters(spec: PaneWindowSpec, w: ViewWindow): Filters {
  return { ...paneFilters(spec), t0: w.t0, t1: w.t1 };
}

/**
 * This pane's **Confirmed** query (T-263, ADR-0017 TM-7): [[paneFilters]] plus `at` — the instant
 * this pane is showing — **always**, following or frozen (T-389).
 *
 * Still no `t0`/`t1`, and that is the whole point. A window *selects* rows, so windowing Confirmed
 * would drop a catalogue entry that happened to be quiet during the window. `at` carries the other
 * half of what a window meant — it scopes the `presence` projection and selects nothing — so the
 * row stays listed and reads the liveness it had *then*.
 *
 * A frozen pane names its own instant; a following pane names the capture clock's live edge, never
 * `Date.now()` (T-379: two clocks on one surface is how a transmitting station came to be listed as
 * silent). `null` — no edge known yet — sends nothing rather than inventing one.
 */
export function paneConfirmedFilters(spec: PaneWindowSpec, edge: number | null): Filters {
  const f = paneFilters(spec);
  const at = !spec.live && spec.tS !== null ? spec.tS : edge;
  if (at !== null) f.at = at;
  return f;
}

/** The frequency range a coverage question about this pane's window is asked over. */
export function paneBand(spec: PaneWindowSpec): { lo: number; hi: number } | null {
  return spec.loHz !== null && spec.hiHz !== null && spec.hiHz > spec.loHz ? { lo: spec.loHz, hi: spec.hiHz } : null;
}

/**
 * A string that changes exactly when the set of pane windows would — which pane is active included.
 *
 * The lists and every pane's boxes are re-asked for on this, not on the time cursor alone: with
 * panes, "the window moved" is a *set* of windows, and a reader watching one of them goes on
 * answering about a pane the user is not looking at. A following pane contributes `live` and its
 * span, never a derived `t0`/`t1`, so a pane tracking the live edge does not re-ask every frame.
 */
export function paneSpecsKey(specs: readonly PaneWindowSpec[], activeId: string | null): string {
  return [activeId ?? "", ...specs.map(specKey)].join(";");
}

/** One spec's identity for [[paneSpecsKey]]. */
export function specKey(s: PaneWindowSpec): string {
  return `${s.id}@${s.n}|${s.loHz}|${s.hiHz}|${s.live ? "live" : s.tS}|${s.spanS}`;
}

/** Do two specs ask the same question? (Position and id aside — those name the pane, not the
 * window.) Used to keep a pane's already-loaded rows across a republish that moved nothing. */
export function sameSpecWindow(a: PaneWindowSpec, b: PaneWindowSpec): boolean {
  return a.loHz === b.loHz && a.hiHz === b.hiHz && a.live === b.live && a.tS === b.tS && a.spanS === b.spanS;
}

/**
 * What the Explore lists call themselves (T-1002): the heading's suffix and each tab's label, for
 * the pane the lists are currently showing — or `null` when there is nothing to disambiguate.
 *
 * The lists follow the ACTIVE pane, so with a split open they are about one of two windows and
 * must say which; the naming is the same the canvas's outline and the map controls use ("pane 2 of
 * 2", `centre/active-pane.ts`), because two namings of one pane is a second thing to get wrong.
 * With one pane they say nothing extra: a badge on the only viewport there is carries no
 * information (docs/23 §10.6 P1).
 */
export interface ListPaneNames { heading: string; confirmed: string; candidate: string }

export function listPaneNames(active: { n: number; count: number; label: string } | null): ListPaneNames | null {
  if (!active || active.count < 2) return null;
  return { heading: `· ${active.label}`, confirmed: `Confirmed · ${active.label}`, candidate: `Candidates · ${active.label}` };
}
