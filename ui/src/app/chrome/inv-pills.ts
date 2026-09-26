// T-997 (MMAP, user 2026-09-25 via the supervisor): the inventory pills.
//
// WHAT THIS REPLACES AND WHY. T-895 collapsed the left inventory column to a chip — a hamburger
// glyph plus "1 cand, 1 conf" — pinned at the LEFT EDGE, MID-HEIGHT. That is exactly where each
// pane's time ruler prints its labels (`surface/hud.ts`: the time axis runs down a pane's left
// edge), so the chip sat over the timeline markers, and a floating puck at mid-height belongs to
// nothing: "I don't like where this is placed, in the vertical center, it overlaps the timeline
// markers and isn't very useful."
//
// THE REDESIGN. The counts are two small pills docked TOP-LEFT in the map's own chrome cluster,
// under Go-to and the nudge row (`map-controls.css`'s left stack), so they are chrome among chrome:
// one fixed position, never mid-height, never over the rulers, and they fade with the rest of the
// cluster (`.map-fade`). There is no hamburger glyph and no left-hand overlay column any more —
// the lists live in the bottom sheet (`index.html`, `sheet.css`), and a pill OPENS THE SHEET ON ITS
// LIST: it selects that list's tab and raises the sheet to at least `half`. One click to the list,
// one more to a row, so nothing the old overlay offered is further than it was.
//
// KEPT FROM T-895: the counts are the inventory slice's own `renderedInventory` collection — the
// very one the lists render and count (T-389's one-collection rule) — so a pill can never disagree
// with the list it opens.
//
// THIN CLIENT: this module reads the store, writes `inventory.tab` and the card's openness (view
// state) and moves the sheet. It holds no client and reaches no route; a press can no more move the radio than a scroll
// can (`ui/test/app-inv-pills.test.ts` asserts that on a spy client).
import type { MountFn } from "../context";
import { renderedInventory, type Row } from "../explore/inventory";
import { openCardOnList, type InventoryTab } from "../explore/slice";
import { h } from "../dom";
import { fillMapInvHome } from "./inv-home";
import { revealFocusSheet } from "./focus-sheet";

export interface SideCounts { candidate: number; confirmed: number }

/** The two lists' counts, from the same filtered collection the lists render. */
export function sideCounts(rows: Readonly<Record<string, Row>>, focusedId: string | null): SideCounts {
  const { listed } = renderedInventory(rows, focusedId);
  return { candidate: listed.candidate.length, confirmed: listed.confirmed.length };
}

/** The short word on a pill — what the user asked for, verbatim ("1 cand · 1 conf"). */
export const PILL_WORD: Record<InventoryTab, string> = { candidate: "cand", confirmed: "conf" };

/** A pill's accessible name: its count in words, and what a press does. */
export function pillLabel(list: InventoryTab, n: number): string {
  const word = list === "candidate" ? "candidate" : "confirmed signal";
  return `${n} ${word}${n === 1 ? "" : "s"} — open the ${list} list`;
}

/**
 * What a press does, as a pure description (the DOM half is three lines below): the list to select
 * and the snap to raise the sheet to. `half` rather than `full` — the map stays the subject
 * (docs/23 §10.3), and a viewer who left the sheet at `full` keeps it (`show` never lowers).
 * T-1026: the card is hidden until something is pressed, so a pill is one of the two things that
 * puts it on screen at all (the other is selecting a feature) — on the list it names.
 */
export const pillAction = (list: InventoryTab) => ({ tab: list, snap: "half" as const });

/** Scroll the lists to the top of the sheet's scrolling body, so a press lands ON the list it
 * named rather than wherever the body was last left. No-op where the API is absent (a fake DOM). */
export function showLists(doc: Pick<Document, "querySelector"> = document): void {
  const inv = doc.querySelector(".sheet-body .side-inv");
  (inv as HTMLElement | null)?.scrollIntoView?.({ block: "nearest" });
}

export const mountInvPills: MountFn = (el, ctx) => {
  void el; // the lists' own slot; the pills live in the map chrome, not beside the lists.
  const made = new Map<InventoryTab, { n: HTMLElement; btn: HTMLElement }>();

  const pill = (list: InventoryTab) => {
    const n = h("b", { class: `map-pill-n ${list}` }, "0");
    const btn = h("button", {
      class: "map-pill", type: "button", "data-list": list,
      onclick: () => {
        const { tab, snap } = pillAction(list);
        ctx.store.set(openCardOnList(tab));
        revealFocusSheet(snap);
        showLists();
      },
    }, n, h("small", {}, PILL_WORD[list]));
    made.set(list, { n, btn });
    return btn;
  };

  fillMapInvHome((home) => {
    made.clear();
    home.replaceChildren(
      pill("candidate"),
      h("span", { class: "map-pill-sep", "aria-hidden": "true" }, "·"),
      pill("confirmed"),
    );
    home.hidden = false;
    paint(ctx.store.get().inventory.rows, ctx.store.get().focus);
  });

  function paint(rows: Readonly<Record<string, Row>>, focus: { kind: string; id?: string }): void {
    const c = sideCounts(rows, focus.kind === "signal" ? (focus.id ?? null) : null);
    for (const list of ["candidate", "confirmed"] as const) {
      const made_ = made.get(list);
      if (!made_) continue;
      const n = list === "candidate" ? c.candidate : c.confirmed;
      made_.n.textContent = String(n);
      const label = pillLabel(list, n);
      made_.btn.setAttribute("aria-label", label);
      made_.btn.setAttribute("title", label);
    }
  }

  ctx.store.select(
    (s) => [s.inventory.rows, s.focus] as const,
    ([rows, focus]) => paint(rows, focus),
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] },
  );
};
