// The context-menu DOM component (T-192): one singleton `role="menu"` popup, created lazily and
// appended to `document.body` on first use (it isn't tied to any single `data-slot`, since it can
// open from the waterfall, the inventory list or the selections list). Open/close and keyboard
// navigation are `model.ts`'s pure `MenuState` reducer; this file only syncs the DOM (position,
// rendered rows, focus, the outside-click/Esc listeners) to that state.
import { h } from "../dom";
import { activeItem, clampMenuPosition, closeMenu, menuKeyDown, openMenu, type MenuItem, type MenuState } from "./model";

export interface MenuHandle {
  open(items: readonly MenuItem[], x: number, y: number): void;
  close(): void;
  /** Whether the menu is currently open (test/inspection use). */
  isOpen(): boolean;
}

let singleton: MenuHandle | null = null;

function build(): MenuHandle {
  const root = h("div", { class: "ctx-menu", role: "menu", hidden: true });
  document.body.append(root);

  let state: MenuState = closeMenu();
  let openerFocus: HTMLElement | null = null;

  function renderItems() {
    root.replaceChildren(...state.items.map((it, i) => h("button", {
      class: `ctx-item${it.danger ? " danger" : ""}${i === state.active ? " active" : ""}`,
      type: "button",
      role: "menuitem",
      tabindex: i === state.active ? "0" : "-1",
      "aria-disabled": it.disabled ? "true" : undefined,
      onclick: () => {
        if (it.disabled) return;
        close();
        it.onSelect();
      },
    }, h("span", { class: "lbl" }, it.label), it.hint ? h("small", {}, it.hint) : null)));
  }

  function focusActive() {
    (root.children[state.active] as HTMLElement | undefined)?.focus();
  }

  function onOutside(e: PointerEvent) {
    if (!root.contains(e.target as Node)) close();
  }

  function teardown() {
    root.hidden = true;
    document.removeEventListener("pointerdown", onOutside, true);
    document.removeEventListener("keydown", onKey, true);
    const f = openerFocus;
    openerFocus = null;
    if (f && document.contains(f)) f.focus();
  }

  function close() {
    if (!state.open) return;
    state = closeMenu();
    teardown();
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      const it = activeItem(state);
      if (it) { close(); it.onSelect(); }
      return;
    }
    if (e.key === "Tab") { close(); return; } // the menu never shares focus with the page behind it
    const next = menuKeyDown(state, e.key);
    if (next === state) return;
    e.preventDefault();
    state = next;
    if (!state.open) { teardown(); return; }
    renderItems();
    focusActive();
  }

  function open(items: readonly MenuItem[], x: number, y: number) {
    state = openMenu(items);
    if (!state.open) return;
    renderItems();
    openerFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    root.hidden = false;
    // Measure at the unclamped position first so `clampMenuPosition` sees the real size.
    root.style.left = "0px";
    root.style.top = "0px";
    const rect = root.getBoundingClientRect();
    const pos = clampMenuPosition({ x, y }, { w: rect.width, h: rect.height }, { w: window.innerWidth, h: window.innerHeight });
    root.style.left = `${pos.x}px`;
    root.style.top = `${pos.y}px`;
    focusActive();
    document.addEventListener("pointerdown", onOutside, true);
    document.addEventListener("keydown", onKey, true);
  }

  return { open, close, isOpen: () => state.open };
}

/** The one context menu this page uses, created on first call. */
export function contextMenu(): MenuHandle {
  singleton ??= build();
  return singleton;
}
