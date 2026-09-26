// T-1005 (MMAP split view): **the split layout's own controls** — a drag handle on every divider,
// a × on every pane — and the pointer position a close is decided by.
//
// Before this ticket the app split side by side only, `PaneModel.setSplitFraction` had no caller
// (a divider could not be moved), a pane closed only through the viewport menu and only when it was
// the active one, and a close made pane 1 active whatever the user was looking at.
//
// **Not a canvas gesture.** The canvas's pan/zoom vocabulary is interpreted in exactly one place
// (`surface/input.ts`, T-412). A divider drag moves no view: it changes the share of the canvas two
// panes get, one number in the pane model's layout tree, which every frame derives both panes'
// rectangles from. So it lives here, on its own handle elements, never on the canvas. View only:
// nothing in this file can reach a route.

import { h } from "../dom";
import type { PaneRect, PaneView } from "../../surface/surface";
import type { SurfacePreview } from "../../surface/preview";
import { closeButtonSpot, outlineBox, type CssBox } from "./active-pane";

export interface SplitChromeOptions {
  canvas: HTMLCanvasElement;
  /** The mounted surface, or null before it boots. */
  preview: () => SurfacePreview | null;
  /** Close pane `id` (the host also drops that pane's layer registry). */
  closePane: (id: string) => void;
}

export interface SplitChrome {
  /** The layer the handles and × buttons live in; the host puts it in the stage over the canvas. */
  readonly el: HTMLElement;
  /** Place the controls from this frame's panes — the surface's `dom` hook calls it per frame. */
  place(panes: readonly PaneView[], hPx: number, dpr: number): void;
  /** The last pointer position seen over the canvas's box, in canvas GL device px, or null when
   * the pointer is elsewhere. Chrome floats over the canvas, so a press on a pane's × or on the
   * viewport menu counts as over it: the pane that grows under that point is the one the user is
   * looking at (`activeAfterClose`). */
  pointerGl(): { x: number; y: number } | null;
}

export function mountSplitChrome(opts: SplitChromeOptions): SplitChrome {
  const el = h("div", { class: "sf-split" });
  /** The stage the layer is mounted in (its parent), where the layout statement is written. */
  const stage = (): HTMLElement => el.parentElement ?? el;

  let lastPointer: { clientX: number; clientY: number } | null = null;
  const pointerGl = () => {
    if (!lastPointer) return null;
    const r = opts.canvas.getBoundingClientRect();
    const cx = lastPointer.clientX - r.left, cy = lastPointer.clientY - r.top;
    if (cx < 0 || cy < 0 || cx > r.width || cy > r.height || r.width <= 0) return null;
    const dpr = opts.canvas.width / r.width;
    return { x: cx * dpr, y: opts.canvas.height - cy * dpr };
  };
  // Capture phase and passive: this only records where the pointer is, it never handles a gesture.
  for (const t of ["pointermove", "pointerdown"] as const) {
    document.addEventListener(t, (e) => { lastPointer = { clientX: e.clientX, clientY: e.clientY }; }, { capture: true, passive: true });
  }
  document.addEventListener("pointerleave", () => { lastPointer = null; });

/**
 * T-1005: the split layout's controls, placed per frame. **A divider drag re-lays-out both panes in
 * the same frame** because there is only one number: the split's fraction in the pane model, from
 * which every frame derives both rectangles (and this placement). The same frame's pane boxes and
 * dividers are stated on `stage().dataset.splitLayout`, so a test reads one consistent layout, not a
 * divider from one frame and a pane from the next. View only: nothing here reaches a route.
 */
let splitKey = "";
let chromeAt = -Infinity;
let chromeSeen: CssBox[] = [];
let dragging: { path: string; paneId: string | null; dir: "columns" | "rows"; parent: PaneRect } | null = null;
const setFrac = (d: { path: string; paneId: string | null }, frac: number) => {
  const m = opts.preview()?.view.panes;
  if (!m) return;
  // A divider with a pane directly on its first side is that pane's edge; one between two splits
  // is addressed by its place in the layout.
  if (d.paneId) m.setSplitFraction(d.paneId, frac);
  else m.setDividerFraction(d.path, frac);
};
const fracAt = (clientX: number, clientY: number, dir: "columns" | "rows", parent: PaneRect) => {
  const r = opts.canvas.getBoundingClientRect();
  const dpr = r.width > 0 ? opts.canvas.width / r.width : 1;
  const x = (clientX - r.left) * dpr, y = opts.canvas.height - (clientY - r.top) * dpr;
  // Columns: the first side is the left. Rows: the first side is the TOP band (GL y grows up).
  return dir === "columns" ? (x - parent.x) / parent.w : (parent.y + parent.h - y) / parent.h;
};
/** The floating chrome's boxes in CSS px from the canvas's top-left: what a pane's × must avoid. */
const chromeBoxes = (): CssBox[] => {
  const base = opts.canvas.getBoundingClientRect();
  const out: CssBox[] = [];
  // A `map-chips` row's chips can wrap outside the row's own box (a phone width), so each chip
  // is measured as well as the row.
  const els = [...Array.from(stage().querySelectorAll(".map-ctl > *, .map-ctl .map-chips > *")), stage().querySelector(".sf-status")];
  for (const el of els) {
    const r = el?.getBoundingClientRect();
    if (r && r.width > 0 && r.height > 0) out.push({ left: r.left - base.left, top: r.top - base.top, width: r.width, height: r.height });
  }
  return out;
};
const place = (panes: readonly PaneView[], hPx: number, dpr: number) => {
  const p = opts.preview();
  const ids = p ? new Set(p.view.panes.list().map((x) => x.id)) : new Set<string>();
  const drawn = panes.filter((v) => ids.has(v.id));
  const divs = p && drawn.length > 1 ? p.view.dividers() : [];
  const order = p ? p.view.panes.list().map((x) => x.id) : [];
  // Each pane's WHOLE rectangle (its trace strip included) from the same layout as the dividers,
  // so pane, divider and pane abut; the × sits on the drawn (data) part, clear of the strip.
  const whole = p && drawn.length > 1 ? p.view.paneRects() : new Map<string, PaneRect>();
  const boxes = drawn.map((v) => ({ n: order.indexOf(v.id) + 1, id: v.id, ...outlineBox(whole.get(v.id) ?? v.rect, hPx, dpr) }));
  // The chrome moves on its own (a chip row appears when the device reports, wraps at a phone
  // width), so it is re-measured on a short timer rather than only on a layout change.
  const now = performance.now();
  if (drawn.length > 1 && now - chromeAt > 250) { chromeAt = now; chromeSeen = chromeBoxes(); }
  const chrome = drawn.length > 1 ? chromeSeen : [];
  const xAt = new Map(drawn.map((v) => [v.id, closeButtonSpot(outlineBox(v.rect, hPx, dpr), chrome)]));
  const dboxes = divs.map((d) => ({ path: d.path, dir: d.dir, frac: d.frac, paneId: d.paneId, parent: d.parent, ...outlineBox(d.rect, hPx, dpr) }));
  const key = JSON.stringify([boxes, [...xAt.values()], dboxes.map(({ parent: _p, ...rest }) => rest)]);
  if (key === splitKey) return;
  splitKey = key;
  stage().dataset.splitLayout = drawn.length > 1 ? JSON.stringify({
    panes: boxes, dividers: dboxes.map(({ parent: _p, paneId: _i, ...rest }) => rest),
  }) : "";
  // Never rebuild under a drag: the handle holding the pointer capture must survive the frames
  // its own drag causes, so only its position moves.
  if (dragging) {
    for (const d of dboxes) {
      const node = el.querySelector<HTMLElement>(`.sf-divider[data-path="${d.path}"]`);
      if (!node) continue;
      if (d.dir === "columns") Object.assign(node.style, { left: `${d.left + d.width / 2}px`, top: `${d.top}px`, height: `${d.height}px` });
      else Object.assign(node.style, { top: `${d.top + d.height / 2}px`, left: `${d.left}px`, width: `${d.width}px` });
      node.setAttribute("aria-valuenow", String(Math.round(d.frac * 100)));
    }
    for (const b of boxes) {
      const node = el.querySelector<HTMLElement>(`.sf-pane-x[data-pane-id="${b.id}"]`);
      const at = xAt.get(b.id);
      if (node && at) Object.assign(node.style, { left: `${at.left}px`, top: `${at.top}px` });
    }
    return;
  }
  const kids: HTMLElement[] = [];
  for (const d of dboxes) {
    const cols = d.dir === "columns";
    const handle = h("div", {
      class: `sf-divider ${d.dir}`, role: "separator", tabindex: "0",
      "aria-orientation": cols ? "vertical" : "horizontal",
      "aria-valuemin": "5", "aria-valuemax": "95", "aria-valuenow": String(Math.round(d.frac * 100)),
      "aria-label": cols ? "Resize the viewports: drag left or right" : "Resize the viewports: drag up or down",
      title: "Drag to resize the viewports (arrow keys too). View only.",
      "data-path": d.path, "data-dir": d.dir,
    });
    Object.assign(handle.style, cols
      ? { left: `${d.left + d.width / 2}px`, top: `${d.top}px`, height: `${d.height}px` }
      : { top: `${d.top + d.height / 2}px`, left: `${d.left}px`, width: `${d.width}px` });
    handle.addEventListener("pointerdown", (e) => {
      if (e.button !== 0) return;
      e.preventDefault(); e.stopPropagation();
      handle.setPointerCapture?.(e.pointerId);
      dragging = { path: d.path, paneId: d.paneId, dir: d.dir, parent: d.parent };
      stage().dataset.dragging = "divider";
    });
    handle.addEventListener("pointermove", (e) => {
      if (!dragging || dragging.path !== d.path) return;
      setFrac(dragging, fracAt(e.clientX, e.clientY, dragging.dir, dragging.parent));
    });
    const end = () => { dragging = null; delete stage().dataset.dragging; };
    handle.addEventListener("pointerup", end);
    handle.addEventListener("pointercancel", end);
    handle.addEventListener("keydown", (e) => {
      const back = cols ? "ArrowLeft" : "ArrowUp", fwd = cols ? "ArrowRight" : "ArrowDown";
      if (e.key !== back && e.key !== fwd) return;
      e.preventDefault();
      setFrac(d, d.frac + (e.key === fwd ? 0.05 : -0.05));
    });
    kids.push(handle);
  }
  if (drawn.length > 1) for (const b of boxes) {
    const x = h("button", {
      type: "button", class: "sf-pane-x", "aria-label": `Close pane ${b.n} of ${boxes.length}`,
      title: `Close pane ${b.n}. The pane under the pointer — else the live one — becomes active. View only.`,
      "data-pane-id": b.id, "data-pane": String(b.n),
    }, "×");
    const at = xAt.get(b.id)!;
    Object.assign(x.style, { left: `${at.left}px`, top: `${at.top}px` });
    x.addEventListener("click", (e) => { e.stopPropagation(); opts.closePane(b.id); });
    kids.push(x);
  }
  // A keyboard resize re-lays-out, which rebuilds the handles: keep focus on the same divider.
  const focused = (document.activeElement as HTMLElement | null)?.closest?.(".sf-divider") as HTMLElement | null;
  const refocus = focused && el.contains(focused) ? focused.dataset.path ?? null : null;
  el.replaceChildren(...kids);
  if (refocus !== null) el.querySelector<HTMLElement>(`.sf-divider[data-path="${refocus}"]`)?.focus();
};

  return { el, place, pointerGl };
}
