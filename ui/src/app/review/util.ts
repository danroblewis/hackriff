// Small DOM helpers shared by the Review drawer's tabs (T-155). No signal logic: table/row
// scaffolding and the same API-error formatting every other panel uses.
import { h } from "../dom";

export function errText(e: unknown): string {
  const anyE = e as { code?: unknown };
  const code = anyE && typeof anyE.code === "string" ? anyE.code : null;
  const msg = e instanceof Error ? e.message : String(e);
  return code ? `${msg} (${code})` : msg;
}

export function td(text: string, cls = ""): HTMLTableCellElement {
  return h("td", cls ? { class: cls } : {}, text);
}

export function table(head: readonly string[], rows: readonly HTMLTableRowElement[]): HTMLTableElement {
  return h("table", { class: "rv-table" },
    h("thead", {}, h("tr", {}, ...head.map((t) => h("th", {}, t)))),
    h("tbody", {}, ...rows));
}
