// Signal inventory table: /api/inventory (T-018 query). Identity gating is done by the server;
// this only renders what it returns (a withheld identity has no value to show). Text goes in via
// textContent only.
import type { Api } from "./main";
import { fmtT, fromUtcInput, utcInput, type HistoryPanel } from "./history";

export interface Row {
  id: string; f_center_hz: number; bandwidth_hz: number; f_lo_hz: number; f_hi_hz: number;
  first_seen_s: number; last_seen_s: number; count: number;
  known_status: "known" | "unexpected-here" | "unknown";
  status: { author: string; reason: string | null; prior_ref: string | null; reason_withheld: boolean } | null;
  tags: string[]; tags_withheld?: boolean; family: string | null;
  identity_scheme: string | null; identity_value?: string; identity_class: string | null; withheld: boolean;
}
interface Page { entries: Row[]; next_cursor: string | null }
type Key = "status" | "freq" | "bw" | "family" | "identity" | "first" | "last" | "count" | "tags";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const val = (id: string) => $<HTMLInputElement>(id).value.trim();
const STATUS_ORDER = { "unexpected-here": 0, unknown: 1, known: 2 };
const identityText = (r: Row) => r.identity_value ?? (r.withheld ? "withheld" : "");

/** What a row's Listen affordance (its own button, or the row click's inspect target) points
 * Listen at: this emitter, by id (T-069). Pure so it's unit-tested without a DOM. */
export function rowListenTarget(r: Pick<Row, "id" | "f_center_hz">): { emitter: string; label: string } {
  return { emitter: r.id, label: `${(r.f_center_hz / 1e6).toFixed(4)} MHz` };
}
const sortValue: Record<Key, (r: Row) => number | string> = {
  status: (r) => STATUS_ORDER[r.known_status] ?? 3,
  freq: (r) => r.f_center_hz, bw: (r) => r.bandwidth_hz, family: (r) => r.family ?? "",
  identity: (r) => `${r.identity_scheme ?? ""} ${identityText(r)}`, first: (r) => r.first_seen_s,
  last: (r) => r.last_seen_s, count: (r) => r.count, tags: (r) => r.tags.join(","),
};

export class InventoryTable {
  private rows: Row[] = [];
  private cursor: string | null = null;
  private query = "";
  private sort: { key: Key; dir: 1 | -1 } = { key: "last", dir: -1 };
  private selected: string | null = null;

  constructor(private api: Api, private history: HistoryPanel, private onSelect: (fLoHz: number, fHiHz: number) => void,
    /** T-069: a row click also opens the inspect panel for that emitter (Listen becomes available there). */
    private onInspect: (r: Row) => void,
    /** T-069: the row's own Listen button, by emitter id. */
    private onListen: (id: string, label: string) => void) {
    $("inv-form").addEventListener("submit", (e) => { e.preventDefault(); void this.load(); });
    $("inv-more").addEventListener("click", () => void this.load(true));
    $("inv-search").addEventListener("input", () => this.render());
    $("inv-use-region").addEventListener("click", () => this.useHistoryRegion());
    $("inv-clear").addEventListener("click", () => {
      for (const id of ["inv-f-lo", "inv-f-hi", "inv-t0", "inv-t1", "inv-tag", "inv-search"]) $<HTMLInputElement>(id).value = "";
      $<HTMLSelectElement>("inv-status").value = "";
      void this.load();
    });
    for (const th of $("inv-table").querySelectorAll<HTMLElement>("th[data-key]")) {
      th.addEventListener("click", () => {
        const key = th.dataset.key as Key;
        this.sort = { key, dir: this.sort.key === key ? (-this.sort.dir as 1 | -1) : key === "last" || key === "count" ? -1 : 1 };
        this.render();
      });
    }
  }

  /** Copies the region-over-time frequency and time window into the filters, then loads. */
  private useHistoryRegion() {
    const r = this.history.region();
    const set = (id: string, v: string) => { $<HTMLInputElement>(id).value = v; };
    if (Number.isFinite(r.fLoHz) && Number.isFinite(r.fHiHz)) { set("inv-f-lo", String(r.fLoHz / 1e6)); set("inv-f-hi", String(r.fHiHz / 1e6)); }
    if (Number.isFinite(r.t0) && Number.isFinite(r.t1)) { set("inv-t0", utcInput(r.t0)); set("inv-t1", utcInput(r.t1)); }
    void this.load();
  }

  private params(): string | null {
    const p = new URLSearchParams();
    const fLo = val("inv-f-lo"), fHi = val("inv-f-hi"), t0 = val("inv-t0"), t1 = val("inv-t1");
    if (fLo || fHi) {
      if (!fLo || !fHi) return "set both f lo and f hi (or neither)";
      p.set("f_lo", String(+fLo * 1e6)); p.set("f_hi", String(+fHi * 1e6));
    }
    if (t0 || t1) {
      if (!t0 || !t1) return "set both from and to (or neither)";
      p.set("t0", String(fromUtcInput(t0))); p.set("t1", String(fromUtcInput(t1)));
    }
    const status = $<HTMLSelectElement>("inv-status").value, tag = val("inv-tag");
    if (status) p.set("status", status);
    if (tag) p.set("tag", tag);
    p.set("limit", "200");
    this.query = p.toString();
    return null;
  }

  async load(more = false) {
    const info = $("inv-info");
    if (!more) {
      const problem = this.params();
      if (problem) { info.textContent = problem; return; }
      this.rows = []; this.cursor = null;
    }
    const q = more && this.cursor ? `${this.query}&cursor=${encodeURIComponent(this.cursor)}` : this.query;
    info.textContent = "loading…";
    try {
      const page = (await this.api(`/api/inventory?${q}`)) as Page;
      this.rows.push(...page.entries);
      this.cursor = page.next_cursor;
      this.render();
    } catch (e) {
      info.textContent = `inventory: ${(e as Error).message}`;
      $("inv-more").hidden = true;
    }
  }

  private render() {
    const needle = val("inv-search").toLowerCase();
    const shown = this.rows.filter((r) => !needle ||
      [r.id, r.identity_scheme, r.identity_value, r.family, ...r.tags].some((s) => s?.toLowerCase().includes(needle)));
    const { key, dir } = this.sort, f = sortValue[key];
    shown.sort((a, b) => { const x = f(a), y = f(b); return (x < y ? -1 : x > y ? 1 : 0) * dir; });
    for (const th of $("inv-table").querySelectorAll<HTMLElement>("th[data-key]")) {
      if (th.dataset.key === key) th.setAttribute("aria-sort", dir > 0 ? "ascending" : "descending");
      else th.removeAttribute("aria-sort");
    }
    $("inv-body").replaceChildren(...shown.map((r) => this.tr(r)));
    const withheld = this.rows.filter((r) => r.withheld).length;
    $("inv-info").textContent = `${shown.length} of ${this.rows.length} loaded${this.cursor ? " (more on the server)" : ""}` +
      `${withheld ? ` · ${withheld} identities withheld` : ""}`;
    $("inv-more").hidden = !this.cursor;
  }

  private tr(r: Row): HTMLTableRowElement {
    const tr = document.createElement("tr");
    const td = (text: string, cls = "", title = "") => {
      const c = document.createElement("td");
      c.textContent = text;
      if (cls) c.className = cls;
      if (title) c.title = title;
      tr.append(c);
      return c;
    };
    const st = td("");
    const badge = document.createElement("span");
    badge.className = `badge st-${r.known_status}`;
    badge.textContent = r.known_status === "unexpected-here" ? "unexpected here" : r.known_status;
    const s = r.status;
    badge.title = !s ? "" : s.reason_withheld ? `${s.author}: reason withheld` : `${s.author}: ${s.reason ?? ""}${s.prior_ref ? ` (${s.prior_ref})` : ""}`;
    st.append(badge);
    td((r.f_center_hz / 1e6).toFixed(4), "num");
    td((r.bandwidth_hz / 1e3).toFixed(1), "num opt");
    td(r.family ?? "—");
    const idCell = td("", "", r.identity_scheme ? `${r.identity_scheme} · class ${r.identity_class ?? "unclassified"}` : "");
    if (r.identity_value !== undefined) idCell.textContent = `${r.identity_scheme}: ${r.identity_value}`;
    else if (r.withheld) {
      const w = document.createElement("span");
      w.className = "withheld";
      w.textContent = "withheld";
      idCell.append(`${r.identity_scheme}: `, w);
    } else idCell.textContent = "—";
    td(fmtT(r.first_seen_s), "opt");
    td(fmtT(r.last_seen_s));
    td(String(r.count), "num");
    const tags = td("", "opt");
    for (const t of r.tags) { const s = document.createElement("span"); s.className = "tag"; s.textContent = t; tags.append(s); }
    const actions = td("", "listen-cell");
    const listenBtn = document.createElement("button");
    listenBtn.type = "button";
    listenBtn.className = "inv-listen";
    listenBtn.textContent = "Listen";
    listenBtn.title = "Listen to this emitter";
    listenBtn.addEventListener("click", (e) => {
      e.stopPropagation(); // don't also trigger the row click below
      const t = rowListenTarget(r);
      this.onListen(t.emitter, t.label);
    });
    actions.append(listenBtn);
    tr.title = r.id;
    if (r.id === this.selected) tr.className = "sel";
    tr.addEventListener("click", () => {
      this.selected = r.id;
      for (const x of $("inv-body").querySelectorAll("tr.sel")) x.classList.remove("sel");
      tr.classList.add("sel");
      // Pad narrow emitters so the region view shows some context around them.
      const pad = Math.max(r.bandwidth_hz, 25e3);
      this.onSelect(r.f_lo_hz - pad, r.f_hi_hz + pad);
      this.onInspect(r); // T-069: also open the inspect panel, enabling Listen for this emitter
    });
    return tr;
  }
}
