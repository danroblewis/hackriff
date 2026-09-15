// Tiny DOM helpers for the MUI app (ADR-0013 §1). Text is always set with textContent, never
// innerHTML, so API strings (labels, reasons, decoded text) can never inject markup.

type Attrs = Record<string, string | number | boolean | null | undefined | ((e: Event) => void)>;
type Child = Node | string | null | undefined | false;

/** Creates an element: `h("button", { class: "mini", onclick: fn, "aria-pressed": true }, "Mute")`. */
export function h<K extends keyof HTMLElementTagNameMap>(tag: K, attrs: Attrs = {}, ...children: Child[]): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (typeof v === "function") el.addEventListener(k.replace(/^on/, ""), v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children) if (c) el.append(c);
  return el;
}

/** The element carrying `data-slot="<name>"` (throws when the page lacks it: a layout bug). */
export function slot(name: string, root: ParentNode = document): HTMLElement {
  const el = root.querySelector<HTMLElement>(`[data-slot="${name}"]`);
  if (!el) throw new Error(`missing data-slot="${name}"`);
  return el;
}

export const byId = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T | null;
