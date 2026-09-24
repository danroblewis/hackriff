// T-895 (user principle P1, docs/23 §10.6 rule 1): the left inventory column (`.side`: the
// Candidate/Confirmed lists and the selections under them) is COLLAPSED BY DEFAULT on every width —
// a chip at the left edge, no taller than 56 px, that states the two lists' counts — so the map is
// what Explore opens on. Pressing the chip opens the lists as an overlay with a visible close (×);
// the close (or Escape) puts every pixel back to the map. Collapsed means GONE, not faded.
//
// Fixed position, never draggable (P3). The chip, and the open overlay, stay clear of the floating
// Go-to, the zoom cluster and the FAB (T-802), the surface's toolbar (T-528) and the sheet's handle
// (T-803): collapsed, the chip sits at mid-height on the left edge; open, the column starts below
// the lowest of the top chrome's bottoms (bar, toolbar, Go-to) — measured here into `--side-top` —
// ends above the sheet's peek strip, and on narrow screens stops short of the right-edge column the
// zoom stack and FAB live in (`map-layout.css`).
//
// THIN CLIENT: the counts are the inventory slice's own `renderedInventory` collection — the very
// one the lists render and count (T-389's one-collection rule) — so the chip can never disagree
// with the list it opens. It reads the store and writes nothing but its own open/closed class; it
// holds no client and reaches no route.
import type { MountFn } from "../context";
import { renderedInventory, type Row } from "../explore/inventory";
import { h } from "../dom";

/** The chip's height bound in CSS px (the ticket's "no taller than 56 px"). `map-layout.css`
 * sizes the chip to fit inside it. */
export const SIDE_CHIP_MAX_PX = 56;
/** Gap between the top chrome and the open overlay on narrow screens, CSS px. */
export const SIDE_TOP_GAP_PX = 8;

export interface SideCounts { candidate: number; confirmed: number }

/** The two lists' counts, from the same filtered collection the lists render. */
export function sideCounts(rows: Readonly<Record<string, Row>>, focusedId: string | null): SideCounts {
  const { listed } = renderedInventory(rows, focusedId);
  return { candidate: listed.candidate.length, confirmed: listed.confirmed.length };
}

/** The chip's accessible name: the counts in words, and what a press does. */
export function sideChipLabel(c: SideCounts): string {
  return `Signal lists: ${c.candidate} candidate${c.candidate === 1 ? "" : "s"}, `
    + `${c.confirmed} confirmed — open the lists over the map`;
}

/**
 * Where the open overlay's top goes on a narrow screen: below the lowest of the top chrome's
 * bottoms (the bar, the surface's toolbar, the floating Go-to), plus a gap. Non-finite readings
 * (an element not built yet) are ignored; null when nothing was measurable, so the CSS fallback
 * stands.
 */
export function sideTopPx(bottoms: readonly (number | null | undefined)[]): number | null {
  const ok = bottoms.filter((b): b is number => typeof b === "number" && Number.isFinite(b));
  return ok.length ? Math.ceil(Math.max(...ok)) + SIDE_TOP_GAP_PX : null;
}

const TOP_CHROME = [".app > .bar", ".sf-bar", ".map-goto"];

export const mountSideChip: MountFn = (el, ctx) => {
  el.classList.add("is-collapsed");
  const cand = h("span", { class: "side-chip-n cand" });
  const conf = h("span", { class: "side-chip-n conf" });
  const chip = h("button", {
    class: "side-chip", type: "button", "aria-expanded": "false",
    onclick: () => setOpen(true),
  }, h("span", { class: "side-chip-ico", "aria-hidden": "true" }, "☰"),
  h("span", { class: "side-chip-counts" },
    h("span", { class: "side-chip-row" }, cand, h("small", {}, " cand")),
    h("span", { class: "side-chip-row" }, conf, h("small", {}, " conf"))));
  const close = h("button", {
    class: "side-close", type: "button", "aria-label": "Close the lists — back to the map",
    title: "Close the lists (Esc)", onclick: () => setOpen(false),
  }, "×");
  const top = h("div", { class: "side-top" }, h("span", { class: "side-top-h" }, "Signal lists"), close);
  el.prepend(chip, top);

  let open = false;
  function place() {
    if (typeof document === "undefined" || !document.querySelector) return;
    const px = sideTopPx(TOP_CHROME.map((sel) => {
      const e = document.querySelector(sel);
      if (!e) return null;
      const r = e.getBoundingClientRect();
      return r.height > 0 ? r.bottom : null;
    }));
    if (px !== null) el.style.setProperty("--side-top", `${px}px`);
  }
  function setOpen(on: boolean) {
    if (on === open) return;
    open = on;
    el.classList.toggle("is-open", on);
    el.classList.toggle("is-collapsed", !on);
    chip.setAttribute("aria-expanded", String(on));
    if (on) { place(); close.focus?.(); } else chip.focus?.();
  }
  if (typeof window !== "undefined") {
    window.addEventListener("resize", () => { if (open) place(); });
    window.addEventListener("keydown", (e: Event) => {
      if (open && (e as KeyboardEvent).key === "Escape") setOpen(false);
    });
  }

  ctx.store.select(
    (s) => [s.inventory.rows, s.focus] as const,
    ([rows, focus]) => {
      const c = sideCounts(rows, focus.kind === "signal" ? focus.id : null);
      cand.textContent = String(c.candidate);
      conf.textContent = String(c.confirmed);
      const label = sideChipLabel(c);
      chip.setAttribute("aria-label", label);
      chip.setAttribute("title", label);
    },
    { immediate: true, eq: (a, b) => a[0] === b[0] && a[1] === b[1] },
  );
};
