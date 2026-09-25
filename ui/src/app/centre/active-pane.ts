// T-1000 (MMAP split view): **the active pane, made visible.**
//
// Panes are independent inside `surface/panes.ts`, but Go-to, zoom, the layers menu, the follow-live
// FAB and the viewport menu's Close / Whole surface all act on ONE pane — `SurfacePreview`'s
// `activePane`, the one last pressed, right-clicked, wheeled or chosen by key. Before this ticket
// nothing on screen said which pane that was, so with two panes open a zoom or a Go-to landed on a
// pane the user could not name. The rule (docs/23 §10.7): **chrome that acts on one pane shows
// which.** This module is the presentation arithmetic behind that: how a pane is named, where its
// outline goes, and what a key press asks for. It names no route and moves no view by itself.

import type { PaneRect } from "../../surface/surface";

/** How the chrome names the active pane: its 1-based position in layout order (left-to-right,
 * top-to-bottom — `PaneModel.list()`), never its internal id, because "pane 2" must be the one the
 * user sees second. */
export interface ActiveName {
  /** 1-based position in layout order. */
  n: number;
  count: number;
  /** "pane 2 of 3". */
  label: string;
}

/**
 * The active pane's name, or `null` when there is nothing to disambiguate — a single pane, or an id
 * that is not (or no longer) a pane. With one pane the chrome says nothing extra: an outline and a
 * badge on the only viewport there is would be overlay with no information in it (docs/23 §10.6 P1).
 */
export function activePaneName(ids: readonly string[], active: string | null): ActiveName | null {
  if (ids.length < 2 || active === null) return null;
  const i = ids.indexOf(active);
  if (i < 0) return null;
  return { n: i + 1, count: ids.length, label: `pane ${i + 1} of ${ids.length}` };
}

/** The pane `step` places after (or before) `active` in layout order, wrapping. `null` when there is
 * no other pane to move to. */
export function stepPane(ids: readonly string[], active: string, step: 1 | -1): string | null {
  if (ids.length < 2) return null;
  const i = ids.indexOf(active);
  const from = i < 0 ? 0 : i;
  return ids[(from + step + ids.length) % ids.length];
}

/** What a key press asks of the panes. */
export type PaneKey =
  | { kind: "step"; step: 1 | -1 }
  | { kind: "index"; n: number }
  | { kind: "live" };

/** The subset of a `KeyboardEvent` [[paneKeyIntent]] reads, so a test can hand it a literal. */
export interface PaneKeyEvent {
  key: string;
  ctrlKey?: boolean; metaKey?: boolean; altKey?: boolean;
  repeat?: boolean;
  defaultPrevented?: boolean;
  target?: unknown;
}

/** Input types a bare letter or digit is NOT text in — pressing `L` on a focused checkbox is still a
 * command, not typing. */
const NON_TEXT_INPUTS = new Set(["checkbox", "radio", "button", "submit", "reset", "range", "color", "file", "image"]);

/** Is the key going into something the user is typing in? Then it is never a pane command. */
export function isTypingTarget(target: unknown): boolean {
  const t = target as { tagName?: unknown; type?: unknown; isContentEditable?: unknown } | null | undefined;
  if (!t || typeof t.tagName !== "string") return false;
  const tag = t.tagName.toUpperCase();
  if (tag === "TEXTAREA" || tag === "SELECT") return true;
  if (tag === "INPUT") return !NON_TEXT_INPUTS.has(String(t.type ?? "text").toLowerCase());
  return t.isContentEditable === true;
}

/**
 * The pane keys (ui/CONTROLS.md, docs/23 §10.7):
 *
 * | Key | Asks |
 * |---|---|
 * | `]` | the next pane becomes active (layout order, wrapping) |
 * | `[` | the previous pane becomes active |
 * | `1`–`9` | pane N becomes active |
 * | `L` | toggle Live on the active pane — the follow-live FAB's press, and exactly as view-only |
 *
 * Bare keys only: with Ctrl, Cmd or Alt held a key is the browser's or the OS's (Cmd+L focuses the
 * address bar; Cmd+1 switches tab), and a held key's auto-repeat is not a second request. A key
 * going into a text field is typing, never a command. Nothing here can reach a device: every answer
 * is view state (which pane is active, whether it follows the live edge).
 */
export function paneKeyIntent(e: PaneKeyEvent): PaneKey | null {
  if (e.defaultPrevented || e.repeat || e.ctrlKey || e.metaKey || e.altKey) return null;
  if (isTypingTarget(e.target)) return null;
  if (e.key === "]") return { kind: "step", step: 1 };
  if (e.key === "[") return { kind: "step", step: -1 };
  if (e.key === "l" || e.key === "L") return { kind: "live" };
  if (/^[1-9]$/.test(e.key)) return { kind: "index", n: Number(e.key) };
  return null;
}

/**
 * The active pane's outline box in CSS px from the canvas's top-left, from the pane's rectangle in
 * drawing-buffer px (GL convention: origin bottom-left) — the same conversion every other per-frame
 * placement on this surface makes (`dpr` = device px per CSS px).
 */
export function outlineBox(rect: PaneRect, canvasHpx: number, dpr: number): { left: number; top: number; width: number; height: number } {
  const k = dpr > 0 ? 1 / dpr : 1;
  return {
    left: rect.x * k,
    top: (canvasHpx - rect.y - rect.h) * k,
    width: rect.w * k,
    height: rect.h * k,
  };
}
