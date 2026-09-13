// Selections list (T-044): rename, delete, zoom the live view, load region history. The demod,
// record and inspect actions are rendered disabled until T-052 wires them. Text via textContent.
import { fmtBandwidth } from "./axis";
import { fmtT } from "./history";
import { MAX_NAME_LEN, SELECTION_ACTIONS, type Selection, type SelectionStore } from "./selections";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

export interface SelectionActions {
  zoom: (s: Selection) => void;
  history: (s: Selection) => void;
  /** Listen to the region (T-043). */
  listen?: (s: Selection) => void;
}

export class SelectionPanel {
  constructor(private store: SelectionStore, private actions: SelectionActions) {
    store.subscribe((list) => this.render(list));
    $("sel-clear").addEventListener("click", () => store.clear());
    this.render(store.list());
  }

  private render(list: readonly Selection[]) {
    $("sel-body").replaceChildren(...list.map((s) => this.tr(s)));
    $("sel-table").hidden = !list.length;
    $("sel-clear").hidden = !list.length;
    $("sel-info").textContent = list.length
      ? `${list.length} region${list.length > 1 ? "s" : ""} (this page only)`
      : "Drag across the spectrum or waterfall to add a region; drag vertically in the waterfall to bound its time as well.";
  }

  private tr(s: Selection): HTMLTableRowElement {
    const tr = document.createElement("tr");
    const td = (text = "", cls = "") => {
      const c = document.createElement("td");
      c.textContent = text;
      if (cls) c.className = cls;
      tr.append(c);
      return c;
    };
    const name = document.createElement("input");
    name.type = "text";
    name.value = s.name;
    name.maxLength = MAX_NAME_LEN;
    name.className = "sel-name";
    name.setAttribute("aria-label", "Selection name");
    name.addEventListener("change", () => { if (!this.store.rename(s.id, name.value)) name.value = s.name; });
    name.addEventListener("keydown", (e) => {
      if (e.key === "Enter") name.blur();
      if (e.key === "Escape") { name.value = s.name; name.blur(); }
    });
    td().append(name);
    td((s.f_lo / 1e6).toFixed(6), "num");
    td((s.f_hi / 1e6).toFixed(6), "num");
    td(fmtBandwidth(s.f_hi - s.f_lo), "num opt");
    td(s.t_lo !== undefined && s.t_hi !== undefined ? `${fmtT(s.t_lo)} +${(s.t_hi - s.t_lo).toFixed(2)} s` : "any", "opt");
    const acts = document.createElement("div"); // flex inside the cell (a flex <td> breaks the table)
    acts.className = "acts";
    td().append(acts);
    const button = (label: string, title: string, onClick?: () => void) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = label;
      b.title = title;
      if (onClick) b.addEventListener("click", onClick);
      else b.disabled = true;
      acts.append(b);
    };
    button("Zoom", "Zoom the live view to this region", () => this.actions.zoom(s));
    button("History", "Load this region (and its time range) in region over time", () => this.actions.history(s));
    const listen = this.actions.listen;
    if (listen) button("Listen", "Demodulate the strongest signal in this region to audio (mode estimated)", () => listen(s));
    for (const a of SELECTION_ACTIONS) button(a.label, `Not wired yet (${a.task})`);
    button("Delete", "Delete this selection", () => this.store.remove(s.id));
    return tr;
  }
}
