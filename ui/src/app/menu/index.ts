// Public surface of the context-menu module (T-192, docs/14-ui-rewrite.md "Added scope from
// docs/15 §7"): right-click/long-press on a waterfall bracket, an inventory row or a selection
// opens the same action menu the old focus-bar buttons carried. Callers (explore/index.ts,
// centre/live-spectrum.ts) resolve a DOM hit to an id, then call `openSignalMenu`/
// `openSelectionMenu` with the pointer position; this module owns building the item list and the
// popup itself.
import type { AppContext } from "../context";
import { foundInside } from "../explore/selections";
import type { Row } from "../explore/inventory";
import type { Selection } from "../explore/selections";
import { signalMenuItems, selectionMenuItems } from "./actions";
import { contextMenu } from "./menu";

export { bindContextTrigger, type TriggerHandler } from "./trigger";
export type { MenuItem } from "./model";

/** Opens the menu for a focused signal at client coordinates `(x, y)`. */
export function openSignalMenu(ctx: AppContext, row: Row, x: number, y: number): void {
  contextMenu().open(signalMenuItems(ctx, row), x, y);
}

/** Opens the menu for a focused selection at client coordinates `(x, y)`; the "found inside" rows
 * (already-loaded inventory only, same source the selection focus panel reads) size "Listen to
 * all". */
export function openSelectionMenu(ctx: AppContext, sel: Selection, x: number, y: number): void {
  const rows = foundInside(sel, Object.values(ctx.store.get().inventory.rows));
  contextMenu().open(selectionMenuItems(ctx, sel, rows), x, y);
}

export { contextMenu, signalMenuItems };
