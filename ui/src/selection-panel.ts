// Selections list (T-044, T-052): server-backed store with a sync notice, rename, delete, tick
// several for bulk actions, zoom the live view, region history, and the per-selection actions
// (Inspect, Listen, Demod, Record; hooks in selections.ts). Text via textContent.
import { fmtBandwidth } from "./axis";
import { fmtT } from "./history";
import { type OutputSession, progressText } from "./outputs";
import {
  type ActionId, type ActionOutcome, type InspectReport, MAX_NAME_LEN, SELECTION_ACTIONS, type Selection,
  type SelectionActionHooks, type SelectionStore, runSelectionAction, sortSelections, syncText,
} from "./selections";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

export interface SelectionActions {
  zoom: (s: Selection) => void;
  history: (s: Selection) => void;
  /** Action hooks (Inspect also loads region history through `history`). */
  hooks: SelectionActionHooks;
  /** Saves the selection's band as a server-side bookmark (T-051). */
  bookmark?: (s: Selection) => void;
  /** Output recordings (T-061): stop control and download links (token appended). */
  outputs?: { stop: (id: string) => Promise<string | null>; href: (url: string) => string };
}

export class SelectionPanel {
  /** Frequency-ascending by default (T-083); kept across reloads (rename/remove/sync all re-render
   * through [[render]] without touching this). */
  private sortDir: 1 | -1 = 1;

  constructor(private store: SelectionStore, private actions: SelectionActions) {
    store.subscribe((list) => this.render(list));
    $("sel-clear").addEventListener("click", () => store.clear());
    $<HTMLInputElement>("sel-all").addEventListener("change", (e) => store.pickAll((e.target as HTMLInputElement).checked));
    $("sel-inspect-picked").addEventListener("click", () => void this.run("inspect", store.pickedList()));
    $("sel-delete-picked").addEventListener("click", () => { for (const s of store.pickedList()) store.remove(s.id); });
    for (const th of $("sel-table").querySelectorAll<HTMLElement>("th[data-key]")) {
      th.addEventListener("click", () => { this.sortDir = this.sortDir === 1 ? -1 : 1; this.render(this.store.list()); });
    }
    this.render(store.list());
  }

  /** Shows an inspection report (emitters inside + top explanations). */
  showReport(r: InspectReport) {
    const box = document.createElement("div");
    box.className = "sel-report";
    const h = document.createElement("h3");
    h.textContent = `${r.selection.name}: ${r.emitters.length}${r.more ? "+" : ""} emitter${r.emitters.length === 1 ? "" : "s"} inside`;
    box.append(h);
    if (!r.emitters.length) {
      const p = document.createElement("p");
      p.className = "hint";
      p.textContent = "No inventory emitter overlaps this region (region history is loaded below).";
      box.append(p);
    }
    const ul = document.createElement("ul");
    for (const e of r.emitters) {
      const li = document.createElement("li");
      const head = document.createElement("span");
      head.textContent = `${(e.f_center_hz / 1e6).toFixed(4)} MHz · ${fmtBandwidth(e.bandwidth_hz)} · ${e.known_status} · ×${e.count}`;
      li.append(head);
      const ex = document.createElement("span");
      ex.className = "hint";
      ex.textContent = e.top.length
        ? ` — ${e.top.map((x) => `${x.rank}. ${x.label} (${Math.round(100 * x.score)} %${x.flags.length ? `, ${x.flags.join(", ")}` : ""})`).join("; ")}`
        : " — no explanation yet";
      li.append(ex);
      ul.append(li);
    }
    box.append(ul);
    $("sel-report").replaceChildren(box);
  }

  /** Output recordings started here: progress, a Stop button while active, download links. */
  showOutputs(list: readonly OutputSession[]) {
    const box = $("sel-outputs");
    const rows = list.map((o) => {
      const row = document.createElement("div");
      row.className = "sel-output";
      const name = this.store.get(o.selection_id ?? "")?.name ?? `${(o.f_lo_hz / 1e6).toFixed(3)} MHz`;
      const text = document.createElement("span");
      text.textContent = `${name}: ${progressText(o)} `;
      row.append(text);
      const outs = this.actions.outputs;
      if (o.active && outs) {
        const b = document.createElement("button");
        b.type = "button";
        b.textContent = "Stop";
        b.title = "Stop this recording (files are finalised)";
        b.addEventListener("click", () => {
          b.disabled = true;
          void outs.stop(o.id).then((err) => { if (err) $("sel-status").textContent = `stop: ${err}`; });
        });
        row.append(b);
      }
      if (outs) {
        for (const f of o.files.filter((x) => x.state !== "refused")) {
          for (const [label, url] of [[f.file, f.url], [f.sidecar, f.sidecar_url]] as const) {
            const a = document.createElement("a");
            a.href = outs.href(url);
            a.textContent = label;
            a.setAttribute("download", label);
            row.append(" ", a);
          }
        }
      }
      return row;
    });
    box.replaceChildren(...rows);
    box.hidden = !rows.length;
  }

  private async run(action: ActionId, targets: readonly Selection[]) {
    if (!targets.length) return;
    if (action === "inspect") for (const s of targets.slice(0, 1)) this.actions.history(s);
    const results = await runSelectionAction(action, targets, this.store, this.actions.hooks);
    this.status(results);
  }

  private status(results: ActionOutcome[]) {
    const el = $("sel-status");
    el.textContent = results.map((r) => r.message).join(" · ");
    el.classList.toggle("bad", results.some((r) => r.status === "failed"));
  }

  private render(list: readonly Selection[]) {
    const picked = this.store.pickedList().length;
    const sorted = sortSelections(list, this.sortDir);
    $("sel-body").replaceChildren(...sorted.map((s) => this.tr(s)));
    const freqTh = $("sel-table").querySelector<HTMLElement>("th[data-key]");
    freqTh?.setAttribute("aria-sort", this.sortDir > 0 ? "ascending" : "descending");
    $("sel-table").hidden = !list.length;
    $("sel-clear").hidden = !list.length;
    $("sel-bulk").hidden = !picked;
    $("sel-bulk-count").textContent = `${picked} ticked`;
    const all = $<HTMLInputElement>("sel-all");
    all.checked = !!list.length && picked === list.length;
    all.indeterminate = picked > 0 && picked < list.length;
    const st = this.store.sync();
    $("sel-info").textContent = list.length || st.mode !== "local"
      ? syncText(st, list.length)
      : "Drag across the spectrum or waterfall to add a region; drag vertically in the waterfall to bound its time as well.";
    $("sel-info").classList.toggle("bad", st.mode === "offline");
  }

  private tr(s: Selection): HTMLTableRowElement {
    const tr = document.createElement("tr");
    if (this.store.isPicked(s.id)) tr.className = "picked";
    const td = (text = "", cls = "") => {
      const c = document.createElement("td");
      c.textContent = text;
      if (cls) c.className = cls;
      tr.append(c);
      return c;
    };
    const tick = document.createElement("input");
    tick.type = "checkbox";
    tick.checked = this.store.isPicked(s.id);
    tick.setAttribute("aria-label", `Tick ${s.name}`);
    tick.addEventListener("change", () => this.store.pick(s.id, tick.checked));
    td().append(tick);
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
    const links = td(s.links.length ? String(s.links.length) : "—", "num opt");
    links.title = s.links.slice(-10).map((l) => `${fmtT(l.t)} ${l.kind} ${l.target}${l.note ? ` (${l.note})` : ""}`).join("\n");
    const acts = document.createElement("div"); // flex inside the cell (a flex <td> breaks the table)
    acts.className = "acts";
    td().append(acts);
    const button = (label: string, title: string, onClick: () => void) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = label;
      b.title = title;
      b.addEventListener("click", onClick);
      acts.append(b);
    };
    button("Zoom", "Zoom the live view to this region", () => this.actions.zoom(s));
    button("History", "Load this region (and its time range) in region over time", () => this.actions.history(s));
    for (const a of SELECTION_ACTIONS) button(a.label, a.title, () => void this.run(a.id, [s]));
    const bookmark = this.actions.bookmark;
    if (bookmark) button("Bookmark", "Save this band as a bookmark (server-side)", () => bookmark(s));
    button("Delete", "Delete this selection", () => this.store.remove(s.id));
    return tr;
  }
}
