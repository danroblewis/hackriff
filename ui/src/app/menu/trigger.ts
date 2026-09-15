// Opens the context menu from a right-click, a keyboard "menu"/Shift+F10 press (the browser
// synthesizes a `contextmenu` event for both, targeted at the actual element), or a touch
// long-press — some mobile browsers never synthesize `contextmenu` from a long-press on a
// non-editable element, so this adds an explicit timer-based fallback (T-192 "long-press opens it
// on touch"). One listener set per container; callers resolve the DOM target to a menu target
// themselves (closest(".row[data-id]") etc.) and simply return when the point hit nothing menu-able.

const LONG_PRESS_MS = 550;
const MOVE_TOLERANCE_PX = 10;

/** Whether a touch that started at `(x0,y0)` and moved to `(x1,y1)` travelled far enough to be a
 * scroll/drag rather than a long-press-in-place, cancelling the pending trigger. Pure so the
 * cancel rule is unit-tested without a touch-capable DOM. */
export function movedPastTolerance(x0: number, y0: number, x1: number, y1: number, tolerance = MOVE_TOLERANCE_PX): boolean {
  return Math.hypot(x1 - x0, y1 - y0) > tolerance;
}

export type TriggerHandler = (x: number, y: number, target: HTMLElement) => void;

/** Binds `onTrigger` to right-click/keyboard-menu (`contextmenu`) and touch long-press on `el`. */
export function bindContextTrigger(el: HTMLElement, onTrigger: TriggerHandler): void {
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    onTrigger(e.clientX, e.clientY, e.target as HTMLElement);
  });

  let timer = 0;
  let start: { x: number; y: number; target: HTMLElement } | null = null;
  const cancel = () => { window.clearTimeout(timer); start = null; };

  el.addEventListener("touchstart", (e) => {
    if (e.touches.length !== 1) { cancel(); return; }
    const t = e.touches[0];
    start = { x: t.clientX, y: t.clientY, target: e.target as HTMLElement };
    timer = window.setTimeout(() => {
      if (!start) return;
      const s = start;
      start = null;
      onTrigger(s.x, s.y, s.target);
    }, LONG_PRESS_MS);
  }, { passive: true });

  el.addEventListener("touchmove", (e) => {
    if (!start) return;
    const t = e.touches[0];
    if (movedPastTolerance(start.x, start.y, t.clientX, t.clientY)) cancel();
  }, { passive: true });

  el.addEventListener("touchend", cancel);
  el.addEventListener("touchcancel", cancel);
}
