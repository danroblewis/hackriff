// Pure model for the right-click / long-press context menu (T-192, docs/14-ui-rewrite.md "Added
// scope from docs/15 §7"). No DOM here, on purpose: open/close, keyboard-index movement and
// viewport-clamping are all plain data transforms, so they're unit-tested directly (no DOM under
// node:test, ADR-0013 §1 / the dock precedent "canvas/WebGL/audio are never tested headless").
// `menu.ts` is the thin DOM component that holds one of these states and syncs it to the popup.

/** One menu row. `onSelect` runs the same action a focus-bar button called before (ADR-0013 §4.5)
 * — this module never invents a new API call itself; `menu/actions.ts` wires each item to an
 * existing helper. */
export interface MenuItem {
  id: string;
  label: string;
  hint?: string;
  danger?: boolean;
  disabled?: boolean;
  onSelect(): void;
}

export interface Point { x: number; y: number }
export interface Size { w: number; h: number }

/** Keeps a `size`-shaped menu anchored near `at` fully inside `viewport`, leaving `margin` px of
 * clearance on every edge — the "stays inside the viewport at 400px width" rule. A menu taller or
 * wider than the viewport (minus margins) is pinned to the margin, never pushed negative. */
export function clampMenuPosition(at: Point, size: Size, viewport: Size, margin = 4): Point {
  const maxX = Math.max(margin, viewport.w - size.w - margin);
  const maxY = Math.max(margin, viewport.h - size.h - margin);
  return { x: Math.min(Math.max(at.x, margin), maxX), y: Math.min(Math.max(at.y, margin), maxY) };
}

/** The next highlighted index for ArrowDown/ArrowUp/Home/End over `items`, skipping disabled rows
 * and wrapping at both ends; any other key leaves `current` unchanged. `-1` (nothing highlighted)
 * moves to the first/last enabled row on Down/Up. Empty or all-disabled lists yield `-1`. */
export function moveActiveIndex(current: number, key: string, items: readonly Pick<MenuItem, "disabled">[]): number {
  const count = items.length;
  if (count === 0) return -1;
  const dir = key === "ArrowDown" || key === "Home" ? 1 : key === "ArrowUp" || key === "End" ? -1 : 0;
  if (dir === 0) return current;
  const start = key === "Home" ? -1 : key === "End" ? count : current;
  let i = start;
  for (let step = 0; step < count; step++) {
    i = (i + dir + count) % count;
    if (!items[i]?.disabled) return i;
  }
  return current;
}

/** The first enabled item's index, or `-1` when the menu is empty or every item is disabled — the
 * index a freshly opened menu highlights. */
export function firstEnabledIndex(items: readonly Pick<MenuItem, "disabled">[]): number {
  return items.findIndex((it) => !it.disabled);
}

// ---- open/close/keyboard state (pure; menu.ts syncs the DOM to it) ----

export interface MenuState { open: boolean; items: readonly MenuItem[]; active: number }

export const CLOSED_MENU: MenuState = { open: false, items: [], active: -1 };

/** Opens with `items`, highlighting the first enabled one. Opening with no items is a no-op (there
 * is nothing to show), so the menu stays closed. */
export function openMenu(items: readonly MenuItem[]): MenuState {
  return items.length === 0 ? CLOSED_MENU : { open: true, items, active: firstEnabledIndex(items) };
}

export const closeMenu = (): MenuState => CLOSED_MENU;

/** Arrow/Home/End moves the highlight; Escape closes. Enter/Space activation is a side effect
 * (`activeItem` below tells the caller what to run) and Tab is a DOM-layer close, so both leave
 * this reducer's state alone; any other key is a no-op. A no-op key on a closed menu is also a
 * no-op (nothing to move). */
export function menuKeyDown(state: MenuState, key: string): MenuState {
  if (!state.open) return state;
  if (key === "Escape") return CLOSED_MENU;
  if (key === "ArrowDown" || key === "ArrowUp" || key === "Home" || key === "End") {
    return { ...state, active: moveActiveIndex(state.active, key, state.items) };
  }
  return state;
}

/** The item Enter/Space/a click on the highlighted row would activate: null when the menu is
 * closed, nothing is highlighted, or the highlighted row is disabled. */
export function activeItem(state: MenuState): MenuItem | null {
  const it = state.open && state.active >= 0 ? state.items[state.active] : undefined;
  return it && !it.disabled ? it : null;
}
