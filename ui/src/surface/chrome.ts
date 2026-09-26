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
 * One preset on a viewport's row (T-496) — a second, discoverable, explicit way to ask for
 * something a host offers as a short list, alongside its own primary control. Same
 * strings-and-a-bit shape as [[RowAction]]; `key` is opaque here — a host-chosen identifier so a
 * press can be routed back to the offer it was drawn from — and this file never reads what it
 * means, the same way it never learns what "Retune" means.
 */
export interface WidthAction extends RowAction {
  readonly key: string;
}

/**
 * Supplies a viewport's **status line** (T-1028) — one sentence about something happening to this
 * viewport right now, or `null` when nothing is. Anonymous exactly as [[RowActionFor]] is: this file
 * does not learn what a retune, a mode or a settle is, it shows the sentence it is handed.
 */
export type StatusFor = (id: string) => string | null;

/** Supplies a viewport's width presets, in a fixed order — `[]` for a viewport that has none (e.g.
 * the map, or a host that offers no width control at all). */
export type WidthActionsFor = (id: string) => readonly WidthAction[];

/**
 * **Which front end this viewport's coverage comes from** (T-1006), as strings and one bit.
 *
 * A pane carries a `device` selector — docs/16 §8: it *"only chooses whose coverage decides its
 * grey"* — and until this ticket nothing on screen said what it was. It is stated on the status row
 * because that row is where every other "what am I actually looking at" fact already lives (the
 * level, the tier, the shadow source).
 *
 * Anonymous in the same way [[RowAction]] is, and for the same structural reason: `device` is an
 * opaque selector string this file never interprets, `label` and `why` are the host's sentences, and
 * `stale` is a bit. Nothing about `device_id`s, drivers, sample rates or the control routes reaches
 * here — which is what keeps this file inside the `/surface.html` preview's import graph while the
 * device-naming code stays out of it (see the T-476 note above).
 */
export interface RowDevice {
  /** The selector as state — a `device_id`, or `"any"`. Set on the element as `data-device`. */
  readonly device: string;
  readonly label: string;
  readonly why: string;
  /** The pane names a front end this run does not hold: shown, never silently reset. */
  readonly stale: boolean;
}

/** Supplies a viewport's device pill, or `null` for a viewport that has none (e.g. the map, or a
 * host that knows of no front ends at all). */
export type RowDeviceFor = (id: string) => RowDevice | null;

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
  /** Where this viewport is looking: frequency window and time window (whose coverage is the
   * `device` pill's, T-1006). */
  readonly headline: string;
  /** The time window the headline rounds, unrounded: absolute capture ns as decimal strings, set on
   * the row as `data-t0-ns` / `data-t1-ns` (same reason `data-tier` is — a test reads state, not a
   * sentence; T-472's alt-wheel probe once read `−23 s` → `−23 s` for a move that did happen). */
  readonly t0Ns: string;
  readonly t1Ns: string;
  /** **The stated level**: the cell size the pixels are made of, and the level indices. */
  readonly level: string;
  /** **Which tier the pane drew from** (T-505): `detail` or `overview`. The honesty tiers are two
   * real tile sources now, so the source is stated beside the level — a wide or deep zoom reads as
   * survey overview rather than as upscaled detail presented as measurement. */
  readonly tier: PaneStatus["tier"];
  /** That tier in a sentence, shown as the level cell's title. */
  readonly tierLabel: string;
  /** What the frame actually drew for it — resident tiles, coarse stand-ins, and not-yet-arrived. */
  readonly counts: string;
  /**
   * **What the last-known (shadow) cells on this viewport were read at** (T-916), or `null` when
   * every shadow here was read at the pane's own level — see [[PaneStatus.shadowLabel]]. Shown on
   * its own line, like `ruler`, because it is a second resolution on one screen and the level cell
   * already speaks for the measured cells.
   */
  readonly shadow: string | null;
  /** The other viewports this one resolved to a different level from. */
  readonly differsFrom: readonly string[];
  /** This viewport's own control, or `null` when it has none (T-476). */
  readonly action: RowAction | null;
  /** Intermediate frequency/time marks between the stated edges (T-459), or `null` when the window
   * is too narrow relative to its own cell to offer one — a readout with nothing to add, not a bug. */
  readonly ruler: string | null;
  /** Capture-width presets (T-496), `[]` when this viewport has none. */
  readonly widths: readonly WidthAction[];
  /** One sentence about what is happening to this viewport now (T-1028: a retune the mode has
   * pending, settling or in flight), or `null` when nothing is. Its own line, like `ruler`. */
  readonly status: string | null;
  /** **Whose coverage decides this viewport's grey** (T-1006), or `null` when the viewport has no
   * device of its own (the minimap) or the host knows of no front end to name. */
  readonly device: RowDevice | null;
}

export interface Readout {
  readonly rows: readonly ReadoutRow[];
  /** §8.5a's sentence, or `null` when every viewport agreed. */
  readonly note: string | null;
}

/**
 * What the frame actually drew for one viewport, in words: resident tiles, coarse stand-ins,
 * not-yet-arrived, behind the edge, never sampled.
 *
 * Exported (T-996) because the scale-bar layer states the same report on its own element's dataset
 * and there must be exactly ONE formatting of it — the map's per-pane block and this readout are two
 * views of one `PaneStatus`, not two derivations.
 */
export function paneCountsText(s: PaneStatus): string {
  return `${s.tiles} tiles · ${s.fallbacks} coarse stand-in${s.fallbacks === 1 ? "" : "s"} · ${s.pending} pending`
    + ` · ${s.behind} behind the edge${s.surveyed ? ` · ${s.surveyed} never sampled` : ""}`
    + `${s.blank ? ` · ${s.blank} drew nothing` : ""}`
    + `${s.shortNs > 0 ? ` · drawn to ${(s.shortNs / 1e9).toFixed(1)} s short of the top` : ""}`;
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
  widthsFor: WidthActionsFor | null = null,
  statusFor: StatusFor | null = null,
  deviceFor: RowDeviceFor | null = null,
): Readout {
  const rows = statuses.map((s): ReadoutRow => {
    const viewport = s.id === minimapId ? "minimap" : "pane";
    return {
      id: s.id,
      viewport,
      following: s.following,
      // T-1006: the raw `device` selector used to be appended here as a bare string when it was not
      // `"any"` — a `device_id` with no word for what it meant, and nothing at all in the common case.
      // It has its own stated pill now (`device` below), so the headline is the window alone.
      headline: [s.freqLabel, s.timeLabel].filter(Boolean).join(" · "),
      t0Ns: String(s.t0Ns),
      t1Ns: String(s.t1Ns),
      level: s.levelLabel,
      tier: s.tier,
      tierLabel: s.tierLabel,
      // The fourth count is appended rather than folded into `pending` (T-532): a tile whose
      // newest rows are not yet in hand HAS arrived, and a readout that called that pending would
      // report the fetch as outstanding. It is also, on a following pane, the one number that says
      // the live edge has stopped keeping up.
      // The fifth count is T-580's, and it is the one that makes a fully-grey pane readable: a
      // place the coverage survey settled as never sampled is drawn without a tile and without a
      // request, so a pane over never-swept spectrum otherwise reports `0 tiles · 0 coarse
      // stand-ins · 0 pending` — identical to a pane that has drawn nothing at all. It is
      // appended rather than folded into any of the others for the same reason `behind` was:
      // "never looked" is not "not arrived yet", and a readout that cannot say which is which is
      // the grey-vs-pending confusion one level up.
      counts: paneCountsText(s),
      // T-916: named beside the counts, because it is the same kind of statement as `level` — what
      // the pixels on this pane were actually measured at. Absent when every shadow here was read
      // at the pane's own level, which is the case the readout has nothing extra to say about.
      shadow: s.shadowLabel,
      differsFrom: s.differsFrom,
      // The map is a viewport, but it is not one you *look* through — it is the thing that says
      // where the panes are — so a control that acts on "this viewport's window" has no meaning on
      // it. Asked per row rather than filtered afterwards, so a host may still refuse one itself.
      action: viewport === "minimap" ? null : actionFor?.(s.id) ?? null,
      // The ruler applies to every viewport alike, minimap included: it is another window, and
      // §8.5a's whole point is that a window is a window whether or not you look *through* it.
      ruler: rulerFor?.(s.id) ?? null,
      // Width presets are a device action too, so the map gets none — same reasoning as `action`.
      widths: viewport === "minimap" ? [] : widthsFor?.(s.id) ?? [],
      // The map is not a window you look through, so nothing acts on it — same reasoning again.
      status: viewport === "minimap" ? null : statusFor?.(s.id) ?? null,
      // The minimap has no device of its own for the same reason it has no retune: it is the thing
      // that says where the panes are, not a window you look through at one radio's coverage.
      device: viewport === "minimap" ? null : deviceFor?.(s.id) ?? null,
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
   * it is a discrete event that no drag can synthesise. `onWidth` is the same shape for T-496's
   * width presets, with which preset named alongside the viewport.
   */
  constructor(
    private readonly root: HTMLElement,
    private readonly onAction: ((id: string) => void) | null = null,
    private readonly onWidth: ((id: string, key: string) => void) | null = null,
  ) {
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
      // The tier on the element as well as in the text, so a stylesheet (and a test) can see which
      // source a viewport was drawn from without parsing a sentence.
      entry.root.setAttribute("data-tier", row.tier);
      entry.root.setAttribute("data-t0-ns", row.t0Ns);
      entry.root.setAttribute("data-t1-ns", row.t1Ns);
      set(entry.cells[0], row.viewport === "minimap" ? `${row.id} (map)` : row.id);
      set(entry.cells[1], row.headline);
      set(entry.cells[2], row.level);
      entry.cells[2].title = row.tierLabel;
      set(entry.cells[3], row.counts);
      // T-459: intermediate marks between the two edges `headline` already states. Its own line
      // (`flex-basis: 100%`, like `why`), hidden rather than emptied when there is nothing to mark.
      entry.ruler.hidden = row.ruler === null;
      if (row.ruler !== null) set(entry.ruler, row.ruler);
      // T-916: the last-known tier's own resolution statement. Hidden — not emptied — when there is
      // nothing to say, exactly as `ruler` is, and marked on the element too so a test (and a
      // stylesheet) reads the state rather than parsing the sentence.
      // T-1028: what is happening to this viewport now. `role="status"` on the element (set once at
      // mint), so a screen reader hears a retune the user's own pan asked for — the one place on this
      // surface where a gesture moves the radio, and therefore the one that must not be silent.
      entry.status.hidden = row.status === null;
      entry.root.setAttribute("data-status", row.status === null ? "" : "busy");
      if (row.status !== null) set(entry.status, row.status);
      entry.shadow.hidden = row.shadow === null;
      entry.root.setAttribute("data-shadow-source", row.shadow === null ? "own-level" : "ladder");
      if (row.shadow !== null) set(entry.shadow, row.shadow);
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
      // T-1006: whose coverage this viewport draws. Marked on the ELEMENT as well as said in the
      // pill (`data-device`, `data-device-stale`) — the same reason `data-tier` and
      // `data-shadow-source` are: a test, and a stylesheet, read the state rather than parsing a
      // sentence, and a `device_id` is exactly the kind of string a sentence mangles.
      const d = row.device;
      entry.device.hidden = !d;
      entry.root.setAttribute("data-device", d ? d.device : "");
      entry.root.setAttribute("data-device-stale", d?.stale ? "true" : "false");
      if (d) {
        set(entry.device, d.label);
        entry.device.title = d.why;
      }
      // T-496: the width presets. Buttons persist and are only ever updated in place (same reason
      // as `action`, above), so the array is grown/shrunk to match rather than rebuilt; each
      // button's `key` is captured in a per-button record its own click listener reads fresh,
      // never a value closed over at mint time — the list can change WHICH preset a given button
      // position represents (a host re-ordering, or a different host on a different mount) without
      // the listener naming a stale one.
      const ws = row.widths;
      while (entry.widthBtns.length < ws.length) {
        const rec: WidthBtn = { el: h("button", { class: "hk-surface-width", type: "button" }) as HTMLButtonElement, key: "" };
        rec.el.addEventListener("click", () => { if (!rec.el.disabled) this.onWidth?.(row.id, rec.key); });
        entry.widthBtns.push(rec);
        entry.widthGroup.append(rec.el);
      }
      while (entry.widthBtns.length > ws.length) {
        entry.widthBtns.pop()!.el.remove();
      }
      entry.widthGroup.hidden = ws.length === 0;
      ws.forEach((w, i) => {
        const rec = entry.widthBtns[i];
        rec.key = w.key;
        set(rec.el, w.label);
        rec.el.title = w.why;
        rec.el.disabled = !w.enabled;
        rec.el.setAttribute("aria-disabled", w.enabled ? "false" : "true");
      });
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
    const shadow = h("span", { class: "hk-surface-shadow-source", hidden: true });
    const status = h("span", { class: "hk-surface-status", role: "status", hidden: true });
    // T-1006. Appended LAST on the row, after every existing element: `ui/e2e/app-surface.e2e.mjs`
    // and `app-trace.e2e.mjs` read the headline as `children[1]`, so the cells' positions are part
    // of this row's contract and a new element goes on the end, placed by the stylesheet.
    const device = h("span", { class: "hk-surface-device", hidden: true });
    const widthGroup = h("div", { class: "hk-surface-widths", hidden: true });
    // The id is captured, not read off the DOM: rows are kept by id and this listener outlives every
    // update, so the press names the viewport the row was minted for and nothing else.
    action.addEventListener("click", () => { if (!action.disabled) this.onAction?.(id); });
    const root = h("div", { class: "hk-surface-viewport" }, ...cells, action, why, widthGroup, ruler, shadow, status, device);
    const entry: Row = { root, cells, why, action, ruler, shadow, status, widthGroup, widthBtns: [], device };
    this.rows.set(id, entry);
    this.list.append(root);
    return entry;
  }
}

interface WidthBtn {
  readonly el: HTMLButtonElement;
  /** Which preset this button currently represents — set fresh every `update()`, read fresh by the
   * click listener, never captured at mint time (see the T-496 comment in `update()`). */
  key: string;
}

interface Row {
  readonly root: HTMLElement;
  readonly cells: HTMLElement[];
  readonly why: HTMLElement;
  readonly action: HTMLButtonElement;
  readonly ruler: HTMLElement;
  readonly shadow: HTMLElement;
  readonly status: HTMLElement;
  readonly widthGroup: HTMLElement;
  readonly widthBtns: WidthBtn[];
  readonly device: HTMLElement;
}

const set = (el: HTMLElement, text: string) => { if (el.textContent !== text) el.textContent = text; };
