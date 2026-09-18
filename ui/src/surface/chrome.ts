// The surface's chrome (T-443): **where the per-pane level is surfaced to a user**.
//
// T-442 built `paneStatuses()` and `levelDivergenceNote()` and volunteered the caveat that nothing
// mounted them, because nothing mounted the surface. This is that mount.
//
// The statement matters because docs/16 §8.5a corrected §8.5: the anti-divergence guarantee is
// *"same ramp, same scale, **stated level**"*, **not** "same picture". Two viewports at different
// `(level_f, level_t)` legitimately differ — a coarser cell is a maximum over more cells — and the
// minimap, being 6 GHz wide, is nearly always one of them. A user who reads that difference as "the
// strip doesn't match the waterfall" will file a bug. So the level is *said*, per viewport, from the
// `PaneReport` the renderer actually drew with, and the note explains the difference when there is
// one (and is `null` when there is not, because then there is nothing to explain).
//
// Split the way this repo splits UI code: [[readoutOf]] is pure and unit-tested; [[SurfaceChrome]]
// is the thin DOM binder, which is not tested headless (ADR-0013 §6, "canvas, WebGL and audio are
// never tested headless" — the same reason the renderer is driven by a stub).
//
// ## T-476: this is also where a **per-pane** control lives
//
// The readout already has exactly the shape a per-pane control needs — one row per viewport,
// rebuilt never and updated in place every frame — so the persistent retune control is a slot on
// the row rather than a new widget beside it. The slot is **deliberately anonymous**: a
// [[RowAction]] is a label, a sentence and an enabled bit, and the click is a callback with a
// viewport id. Nothing about tuning, capture configuration or device routes reaches this file.
//
// That is not tidiness. `ui/test/surface-preview.test.ts` asserts that `retune.ts` is **not** in the
// `/surface.html` preview's import graph — T-444's device action is unmounted there — and this file
// *is* in that graph. Typing the slot against `PaneRetuneOffer` would pull the device action into a
// page that must not be able to reach one. The host that knows about retuning supplies the strings.

import { h } from "../app/dom";
import { levelDivergenceNote, type PaneStatus } from "./panes";

/**
 * A control the host puts on one viewport's row, as **strings and a bit** (T-476).
 *
 * `enabled: false` is a *stated* refusal, never a hidden control: `why` is shown next to the button
 * and as its tooltip, so a viewport that cannot do the thing says which thing and why. "Nothing said
 * is never permissive" — a control that vanishes teaches the user nothing, which is the complaint
 * that produced this ticket.
 */
export interface RowAction {
  /** The button's text. Short: the sentence is `why`. */
  readonly label: string;
  /** The whole sentence — what pressing would do, or why it cannot be pressed. */
  readonly why: string;
  readonly enabled: boolean;
}

/** Supplies a viewport's control, or `null` for a viewport that has none (e.g. the map). */
export type RowActionFor = (id: string) => RowAction | null;

/**
 * Supplies a viewport's ruler line (T-459) — the intermediate frequency/time marks between the
 * window's stated edges, or `null` when there is nothing to mark. Anonymous the same way
 * [[RowActionFor]] is: this file does not compute a tick, it only shows the sentence it is handed.
 * `./ticks.ts` derives the sentence from the pane's own box and the `(cellHz, cellS)` the chrome
 * already reports — the caller supplies both, never this file.
 */
export type RulerFor = (id: string) => string | null;

/** One viewport's line of chrome. Strings only: every number was formatted by `paneStatuses`. */
export interface ReadoutRow {
  readonly id: string;
  /** The minimap is a viewport too — it is in the same list, marked, not in a widget of its own. */
  readonly viewport: "pane" | "minimap";
  readonly following: boolean;
  /** Where this viewport is looking: frequency window, time window, and whose coverage. */
  readonly headline: string;
  /** **The stated level**: the cell size the pixels are made of, and the level indices. */
  readonly level: string;
  /** What the frame actually drew for it — resident tiles, coarse stand-ins, and not-yet-arrived. */
  readonly counts: string;
  /** The other viewports this one resolved to a different level from. */
  readonly differsFrom: readonly string[];
  /** This viewport's own control, or `null` when it has none (T-476). */
  readonly action: RowAction | null;
  /** Intermediate frequency/time marks between the stated edges (T-459), or `null` when the window
   * is too narrow relative to its own cell to offer one — a readout with nothing to add, not a bug. */
  readonly ruler: string | null;
}

export interface Readout {
  readonly rows: readonly ReadoutRow[];
  /** §8.5a's sentence, or `null` when every viewport agreed. */
  readonly note: string | null;
}

/**
 * The chrome's view model, from the statuses the renderer reported.
 *
 * `minimapId` names which row is the map. It is a label, not a separate code path: the minimap's
 * status was produced by the same `paneStatuses()` call over the same `PaneReport[]`, which is what
 * makes "another viewport" true in the readout as well as in the renderer.
 */
export function readoutOf(
  statuses: readonly PaneStatus[],
  minimapId: string | null = null,
  actionFor: RowActionFor | null = null,
  rulerFor: RulerFor | null = null,
): Readout {
  const rows = statuses.map((s): ReadoutRow => {
    const viewport = s.id === minimapId ? "minimap" : "pane";
    return {
      id: s.id,
      viewport,
      following: s.following,
      headline: [s.freqLabel, s.timeLabel, s.device === "any" ? null : s.device].filter(Boolean).join(" · "),
      level: s.levelLabel,
      counts: `${s.tiles} tiles · ${s.fallbacks} coarse stand-in${s.fallbacks === 1 ? "" : "s"} · ${s.pending} pending`,
      differsFrom: s.differsFrom,
      // The map is a viewport, but it is not one you *look* through — it is the thing that says
      // where the panes are — so a control that acts on "this viewport's window" has no meaning on
      // it. Asked per row rather than filtered afterwards, so a host may still refuse one itself.
      action: viewport === "minimap" ? null : actionFor?.(s.id) ?? null,
      // The ruler applies to every viewport alike, minimap included: it is another window, and
      // §8.5a's whole point is that a window is a window whether or not you look *through* it.
      ruler: rulerFor?.(s.id) ?? null,
    };
  });
  return { rows, note: levelDivergenceNote(statuses) };
}

/**
 * Binds a [[Readout]] to the DOM, one row per viewport plus the divergence note.
 *
 * Rows are kept by id and their text updated in place, so a frame-rate update does not rebuild the
 * subtree sixty times a second. Text is set with `textContent` (never `innerHTML`), like the rest of
 * the app's DOM.
 */
export class SurfaceChrome {
  private readonly rows = new Map<string, Row>();
  private readonly list: HTMLElement;
  private readonly note: HTMLElement;

  /**
   * `onAction` is the press. It is an **ordinary click listener on a button**, never a threshold on
   * a pointer stream — T-407 found two ways a finger could retune a radio, both of them a continuous
   * gesture being read as a committing act, and a persistent control is only safe because pressing
   * it is a discrete event that no drag can synthesise.
   */
  constructor(private readonly root: HTMLElement, private readonly onAction: ((id: string) => void) | null = null) {
    this.list = h("div", { class: "hk-surface-viewports" });
    this.note = h("p", { class: "hk-surface-level-note", hidden: true });
    this.root.append(this.list, this.note);
  }

  update(r: Readout): void {
    const seen = new Set<string>();
    for (const row of r.rows) {
      seen.add(row.id);
      let entry = this.rows.get(row.id);
      if (!entry) entry = this.mint(row.id);
      entry.root.setAttribute("data-viewport", row.viewport);
      entry.root.setAttribute("data-following", row.following ? "true" : "false");
      set(entry.cells[0], row.viewport === "minimap" ? `${row.id} (map)` : row.id);
      set(entry.cells[1], row.headline);
      set(entry.cells[2], row.level);
      set(entry.cells[3], row.counts);
      // T-459: intermediate marks between the two edges `headline` already states. Its own line
      // (`flex-basis: 100%`, like `why`), hidden rather than emptied when there is nothing to mark.
      entry.ruler.hidden = row.ruler === null;
      if (row.ruler !== null) set(entry.ruler, row.ruler);
      // The control is created once with the row and only ever *updated*: a button rebuilt each
      // frame is a button that cannot be pressed, because the element under the finger between
      // pointerdown and pointerup would be a different one.
      const a = row.action;
      entry.action.hidden = !a;
      entry.why.hidden = !a;
      if (a) {
        set(entry.action, a.label);
        set(entry.why, a.why);
        entry.action.title = a.why;
        entry.action.disabled = !a.enabled;
        entry.action.setAttribute("aria-disabled", a.enabled ? "false" : "true");
      }
    }
    for (const [id, entry] of this.rows) {
      if (seen.has(id)) continue;
      entry.root.remove();
      this.rows.delete(id);
    }
    set(this.note, r.note ?? "");
    this.note.hidden = r.note === null;
  }

  dispose(): void {
    this.list.remove();
    this.note.remove();
    this.rows.clear();
  }

  /** Build one viewport's row, with its control wired once. */
  private mint(id: string): Row {
    const cells = [
      h("span", { class: "hk-surface-id" }),
      h("span", { class: "hk-surface-where" }),
      h("span", { class: "hk-surface-level" }),
      h("span", { class: "hk-surface-counts" }),
    ];
    const why = h("span", { class: "hk-surface-why", hidden: true });
    const action = h("button", { class: "hk-surface-action", type: "button", hidden: true }) as HTMLButtonElement;
    const ruler = h("span", { class: "hk-surface-ruler", hidden: true });
    // The id is captured, not read off the DOM: rows are kept by id and this listener outlives every
    // update, so the press names the viewport the row was minted for and nothing else.
    action.addEventListener("click", () => { if (!action.disabled) this.onAction?.(id); });
    const root = h("div", { class: "hk-surface-viewport" }, ...cells, action, why, ruler);
    const entry: Row = { root, cells, why, action, ruler };
    this.rows.set(id, entry);
    this.list.append(root);
    return entry;
  }
}

interface Row {
  readonly root: HTMLElement;
  readonly cells: HTMLElement[];
  readonly why: HTMLElement;
  readonly action: HTMLButtonElement;
  readonly ruler: HTMLElement;
}

const set = (el: HTMLElement, text: string) => { if (el.textContent !== text) el.textContent = text; };
