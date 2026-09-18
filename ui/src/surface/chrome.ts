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

import { h } from "../app/dom";
import { levelDivergenceNote, type PaneStatus } from "./panes";

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
export function readoutOf(statuses: readonly PaneStatus[], minimapId: string | null = null): Readout {
  const rows = statuses.map((s): ReadoutRow => ({
    id: s.id,
    viewport: s.id === minimapId ? "minimap" : "pane",
    following: s.following,
    headline: [s.freqLabel, s.timeLabel, s.device === "any" ? null : s.device].filter(Boolean).join(" · "),
    level: s.levelLabel,
    counts: `${s.tiles} tiles · ${s.fallbacks} coarse stand-in${s.fallbacks === 1 ? "" : "s"} · ${s.pending} pending`,
    differsFrom: s.differsFrom,
  }));
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
  private readonly rows = new Map<string, { root: HTMLElement; cells: HTMLElement[] }>();
  private readonly list: HTMLElement;
  private readonly note: HTMLElement;

  constructor(private readonly root: HTMLElement) {
    this.list = h("div", { class: "hk-surface-viewports" });
    this.note = h("p", { class: "hk-surface-level-note", hidden: true });
    this.root.append(this.list, this.note);
  }

  update(r: Readout): void {
    const seen = new Set<string>();
    for (const row of r.rows) {
      seen.add(row.id);
      let entry = this.rows.get(row.id);
      if (!entry) {
        const cells = [
          h("span", { class: "hk-surface-id" }),
          h("span", { class: "hk-surface-where" }),
          h("span", { class: "hk-surface-level" }),
          h("span", { class: "hk-surface-counts" }),
        ];
        const root = h("div", { class: "hk-surface-viewport" }, ...cells);
        entry = { root, cells };
        this.rows.set(row.id, entry);
        this.list.append(root);
      }
      entry.root.setAttribute("data-viewport", row.viewport);
      entry.root.setAttribute("data-following", row.following ? "true" : "false");
      set(entry.cells[0], row.viewport === "minimap" ? `${row.id} (map)` : row.id);
      set(entry.cells[1], row.headline);
      set(entry.cells[2], row.level);
      set(entry.cells[3], row.counts);
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
}

const set = (el: HTMLElement, text: string) => { if (el.textContent !== text) el.textContent = text; };
