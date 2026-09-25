// T-900 (user principle P1, docs/23 §10.6 rule 1): overlays are CLOSED, not faded. Every band-2/3
// overlay over the map (the bottom sheet, the left column, the layers panel, the Research slide-in
// when it lands, the retune offer) has a visible dismiss of its own; this module is the one place
// Escape is decided, so a single Esc closes exactly ONE overlay — the topmost, i.e. the one opened
// most recently — instead of every overlay that happened to add its own window listener.
//
// An overlay reports itself `open(true)` when it starts covering the map and `open(false)` when it
// stops; re-reporting "open" keeps its place in the stack (a resize re-apply must not bring the sheet
// back to the top). Escape pops the top entry and calls its `close`, which is the same function its
// visible × runs. A key a focused widget already handled (`defaultPrevented`, e.g. a context menu's
// own Escape) is left alone.
//
// Presentation only (CLAUDE.md thin client): no client, no fetch, no route.

export interface OverlayHandle {
  /** Report whether this overlay currently covers the map. */
  open(on: boolean): void;
}

interface Entry { id: string; close: () => void }

/** The open overlays, bottom → top by open order. Pure; node-testable. */
export class OverlayStack {
  private stack: Entry[] = [];

  /** Mark `id` open; an already-open overlay keeps its place. */
  opened(id: string, close: () => void): void {
    const e = this.stack.find((x) => x.id === id);
    if (e) e.close = close;
    else this.stack.push({ id, close });
  }

  closed(id: string): void {
    this.stack = this.stack.filter((x) => x.id !== id);
  }

  /** The topmost open overlay's id, or null. */
  top(): string | null {
    return this.stack.length ? this.stack[this.stack.length - 1].id : null;
  }

  ids(): string[] {
    return this.stack.map((x) => x.id);
  }

  /** Close the topmost overlay. False when nothing was open (Escape is then not ours). */
  escape(): boolean {
    const e = this.stack.pop();
    if (!e) return false;
    e.close();
    return true;
  }
}

/** The page's one stack. */
export const overlays = new OverlayStack();

const wired = new WeakSet<object>();

/** Install the single Escape listener on `win` (once per window object). */
export function wireEscape(win: Pick<Window, "addEventListener"> | undefined, stack: OverlayStack = overlays): void {
  if (!win || wired.has(win)) return;
  wired.add(win);
  win.addEventListener("keydown", (ev: Event) => {
    const k = ev as KeyboardEvent;
    if (k.key !== "Escape" || k.defaultPrevented) return;
    if (stack.escape()) k.preventDefault?.();
  });
}

/**
 * Register an overlay under `id` whose visible dismiss runs `close`. Returns the handle it reports
 * its open state through.
 */
export function trackOverlay(id: string, close: () => void, stack: OverlayStack = overlays): OverlayHandle {
  wireEscape(typeof window === "undefined" ? undefined : window, stack);
  return { open: (on) => (on ? stack.opened(id, close) : stack.closed(id)) };
}
