// Signal inventory: two lists, candidates and confirmed, over /api/inventory?state=... (T-018
// query; T-080 split + promote/delete). Identity gating is done by the server; this only renders
// what it returns (a withheld identity has no value to show). Text goes in via textContent only.
import { fromUtcInput, utcInput, type HistoryPanel } from "./history";

export type EntryState = "candidate" | "confirmed" | "deleted";

/** hk-model recurrence stats (docs/api.md `GET /api/inventory`); shown as the API's own numbers,
 * with no client-side interpretation. */
export interface Recurrence { occurrences: number; appearances: number; span_s: number; on_air_s: number; duty_cycle: number }

export interface Row {
  id: string; state: EntryState;
  f_center_hz: number; bandwidth_hz: number; f_lo_hz: number; f_hi_hz: number;
  first_seen_s: number; last_seen_s: number; count: number;
  known_status: "known" | "unexpected-here" | "unknown";
  status: { author: string; reason: string | null; prior_ref: string | null; reason_withheld: boolean } | null;
  tags: string[]; tags_withheld?: boolean; family: string | null;
  identity_scheme: string | null; identity_value?: string; identity_class: string | null; withheld: boolean;
  recurrence: Recurrence | null;
}
interface Page { entries: Row[]; next_cursor: string | null }
type Key = "status" | "freq" | "bw" | "family" | "identity" | "count" | "tags";

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
  identity: (r) => `${r.identity_scheme ?? ""} ${identityText(r)}`,
  count: (r) => r.count, tags: (r) => r.tags.join(","),
};

/** Shared filter fields (frequency, time, status, tag): applied to both the candidate and
 * confirmed queries alike. */
export interface Filters { fLoHz?: number; fHiHz?: number; t0?: number; t1?: number; status?: string; tag?: string }

/** The `/api/inventory` query for one state's list (T-080: candidates and confirmed load
 * separately — `state=candidate` / `state=confirmed` — never the default combined page). */
export function inventoryQuery(state: "candidate" | "confirmed", f: Filters, cursor?: string | null): string {
  const p = new URLSearchParams();
  p.set("state", state);
  if (f.fLoHz !== undefined) p.set("f_lo", String(f.fLoHz));
  if (f.fHiHz !== undefined) p.set("f_hi", String(f.fHiHz));
  if (f.t0 !== undefined) p.set("t0", String(f.t0));
  if (f.t1 !== undefined) p.set("t1", String(f.t1));
  if (f.status) p.set("status", f.status);
  if (f.tag) p.set("tag", f.tag);
  p.set("limit", "200");
  if (cursor) p.set("cursor", cursor);
  return `/api/inventory?${p}`;
}

/** Minimal server surface the table needs: list rows, promote a candidate, delete an entry — all
 * Bearer-authenticated by the client (controls/client.ts `ControlClient`, which this matches). */
export interface InventoryClient {
  get<T = unknown>(path: string): Promise<T>;
  post<T = unknown>(path: string, body?: unknown): Promise<T>;
  del<T = unknown>(path: string): Promise<T>;
}

/** Loads one page for one state. Pure aside from the client call, so the split-by-state behaviour
 * is unit-tested without a DOM. */
export async function loadInventoryPage(client: InventoryClient, state: "candidate" | "confirmed", f: Filters, cursor?: string | null): Promise<Page> {
  return client.get<Page>(inventoryQuery(state, f, cursor));
}

/** The server's `{"error", "code"}` body (docs/api.md), shown verbatim; falls back to a plain message. */
function errText(e: unknown): string {
  const anyE = e as { code?: unknown; message?: unknown };
  const code = anyE && typeof anyE.code === "string" ? anyE.code : null;
  const msg = e instanceof Error ? e.message : String(e);
  return code ? `${msg} (${code})` : msg;
}

export type ActionResult = { ok: true } | { ok: false; message: string };

/** Promotes a candidate to confirmed, then reloads (T-080). Reload runs only on success: a
 * refused promote changes nothing server-side, so the error stays on screen instead of being
 * overwritten by a reload. */
export async function promoteEntry(client: InventoryClient, id: string, reload: () => Promise<void>): Promise<ActionResult> {
  try {
    await client.post(`/api/inventory/${encodeURIComponent(id)}/promote`);
    await reload();
    return { ok: true };
  } catch (e) {
    return { ok: false, message: errText(e) };
  }
}

/** Deletes an entry, then reloads (T-080). Same reload-on-success-only rule as [[promoteEntry]]. */
export async function deleteEntry(client: InventoryClient, id: string, reload: () => Promise<void>): Promise<ActionResult> {
  try {
    await client.del(`/api/inventory/${encodeURIComponent(id)}`);
    await reload();
    return { ok: true };
  } catch (e) {
    return { ok: false, message: errText(e) };
  }
}

/** Recurrence as plain text: the API's own occurrences/duty-cycle numbers, no added label. */
function recurrenceText(r: Recurrence | null): string {
  if (!r) return "—";
  return `${r.occurrences} occurrences · ${(r.duty_cycle * 100).toFixed(0)}% duty`;
}

interface ListState { rows: Row[]; cursor: string | null; sort: { key: Key; dir: 1 | -1 }; selected: string | null }

/** One state's list (candidates or confirmed): loads, sorts, filters by the shared search box,
 * and renders its own table, actions and info line. */
class InventoryList {
  private st: ListState = { rows: [], cursor: null, sort: { key: "count", dir: -1 }, selected: null };

  constructor(
    private state: "candidate" | "confirmed",
    private client: InventoryClient,
    private filters: () => Filters,
    private search: () => string,
    private onSelect: (fLoHz: number, fHiHz: number) => void,
    private onInspect: (r: Row) => void,
    private onListen: (id: string, label: string) => void,
    /** Called after a successful promote/delete so the sibling list can reload too (a promoted
     * candidate leaves this list and appears in the other). */
    private onChanged: () => void,
  ) {
    const p = this.prefix;
    $(`${p}-more`).addEventListener("click", () => void this.load(true));
    for (const th of $(`${p}-table`).querySelectorAll<HTMLElement>("th[data-key]")) {
      th.addEventListener("click", () => {
        const key = th.dataset.key as Key;
        this.st.sort = { key, dir: this.st.sort.key === key ? (-this.st.sort.dir as 1 | -1) : key === "count" ? -1 : 1 };
        this.render();
      });
    }
  }

  private get prefix() { return `inv-${this.state}`; }

  async load(more = false) {
    const info = $(`${this.prefix}-info`);
    if (!more) { this.st.rows = []; this.st.cursor = null; }
    info.textContent = "loading…";
    try {
      const page = await loadInventoryPage(this.client, this.state, this.filters(), more ? this.st.cursor : null);
      this.st.rows.push(...page.entries);
      this.st.cursor = page.next_cursor;
      this.render();
    } catch (e) {
      info.textContent = `${this.state}: ${errText(e)}`;
      $(`${this.prefix}-more`).hidden = true;
    }
  }

  render() {
    const needle = this.search();
    const shown = this.st.rows.filter((r) => !needle ||
      [r.id, r.identity_scheme, r.identity_value, r.family, ...r.tags].some((s) => s?.toLowerCase().includes(needle)));
    const { key, dir } = this.st.sort, f = sortValue[key];
    shown.sort((a, b) => { const x = f(a), y = f(b); return (x < y ? -1 : x > y ? 1 : 0) * dir; });
    const table = $(`${this.prefix}-table`);
    for (const th of table.querySelectorAll<HTMLElement>("th[data-key]")) {
      if (th.dataset.key === key) th.setAttribute("aria-sort", dir > 0 ? "ascending" : "descending");
      else th.removeAttribute("aria-sort");
    }
    $(`${this.prefix}-body`).replaceChildren(...shown.map((r) => this.tr(r)));
    const withheld = this.st.rows.filter((r) => r.withheld).length;
    $(`${this.prefix}-info`).textContent = `${shown.length} of ${this.st.rows.length} loaded${this.st.cursor ? " (more on the server)" : ""}` +
      `${withheld ? ` · ${withheld} identities withheld` : ""}`;
    $(`${this.prefix}-more`).hidden = !this.st.cursor;
  }

  private async promote(r: Row) {
    const res = await promoteEntry(this.client, r.id, async () => { await this.load(); this.onChanged(); });
    if (!res.ok) $(`${this.prefix}-info`).textContent = `promote ${r.id}: ${res.message}`;
  }

  private async delete(r: Row) {
    const res = await deleteEntry(this.client, r.id, async () => { await this.load(); this.onChanged(); });
    if (!res.ok) $(`${this.prefix}-info`).textContent = `delete ${r.id}: ${res.message}`;
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
    td(String(r.count), "num");
    td(recurrenceText(r.recurrence), "opt");
    const tags = td("", "opt");
    for (const t of r.tags) { const s2 = document.createElement("span"); s2.className = "tag"; s2.textContent = t; tags.append(s2); }
    const actions = td("", "inv-actions");
    const button = (label: string, title: string, onClick: (e: Event) => void) => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = label;
      b.title = title;
      b.addEventListener("click", onClick);
      actions.append(b);
      return b;
    };
    if (this.state === "candidate") {
      button("Promote", "Promote to confirmed", (e) => { e.stopPropagation(); void this.promote(r); });
    }
    button("Delete", "Delete this entry (a later sighting starts a new candidate)", (e) => { e.stopPropagation(); void this.delete(r); });
    button("Listen", "Listen to this emitter", (e) => {
      e.stopPropagation();
      const t = rowListenTarget(r);
      this.onListen(t.emitter, t.label);
    });
    tr.title = r.id;
    if (r.id === this.st.selected) tr.className = "sel";
    tr.addEventListener("click", () => {
      this.st.selected = r.id;
      for (const x of $(`${this.prefix}-body`).querySelectorAll("tr.sel")) x.classList.remove("sel");
      tr.classList.add("sel");
      // Pad narrow emitters so the region view shows some context around them.
      const pad = Math.max(r.bandwidth_hz, 25e3);
      this.onSelect(r.f_lo_hz - pad, r.f_hi_hz + pad);
      this.onInspect(r); // T-069: also open the inspect panel, enabling Listen for this emitter
    });
    return tr;
  }
}

/** The signal inventory panel: candidates and confirmed lists over `/api/inventory` (T-018,
 * T-080), sharing one filter form and search box. */
export class InventoryTable {
  private candidates: InventoryList;
  private confirmed: InventoryList;

  constructor(client: InventoryClient, private history: HistoryPanel,
    onSelect: (fLoHz: number, fHiHz: number) => void,
    onInspect: (r: Row) => void,
    onListen: (id: string, label: string) => void) {
    const filters = () => this.currentFilters();
    const search = () => val("inv-search").toLowerCase();
    this.candidates = new InventoryList("candidate", client, filters, search, onSelect, onInspect, onListen, () => void this.confirmed.load());
    this.confirmed = new InventoryList("confirmed", client, filters, search, onSelect, onInspect, onListen, () => void this.candidates.load());

    $("inv-form").addEventListener("submit", (e) => { e.preventDefault(); void this.load(); });
    $("inv-search").addEventListener("input", () => { this.candidates.render(); this.confirmed.render(); });
    $("inv-use-region").addEventListener("click", () => this.useHistoryRegion());
    $("inv-clear").addEventListener("click", () => {
      for (const id of ["inv-f-lo", "inv-f-hi", "inv-t0", "inv-t1", "inv-tag", "inv-search"]) $<HTMLInputElement>(id).value = "";
      $<HTMLSelectElement>("inv-status").value = "";
      void this.load();
    });
  }

  /** Copies the region-over-time frequency and time window into the filters, then loads. */
  private useHistoryRegion() {
    const r = this.history.region();
    const set = (id: string, v: string) => { $<HTMLInputElement>(id).value = v; };
    if (Number.isFinite(r.fLoHz) && Number.isFinite(r.fHiHz)) { set("inv-f-lo", String(r.fLoHz / 1e6)); set("inv-f-hi", String(r.fHiHz / 1e6)); }
    if (Number.isFinite(r.t0) && Number.isFinite(r.t1)) { set("inv-t0", utcInput(r.t0)); set("inv-t1", utcInput(r.t1)); }
    void this.load();
  }

  private currentFilters(): Filters {
    const fLo = val("inv-f-lo"), fHi = val("inv-f-hi"), t0 = val("inv-t0"), t1 = val("inv-t1");
    const f: Filters = {};
    if (fLo && fHi) { f.fLoHz = +fLo * 1e6; f.fHiHz = +fHi * 1e6; }
    if (t0 && t1) { f.t0 = fromUtcInput(t0); f.t1 = fromUtcInput(t1); }
    const status = $<HTMLSelectElement>("inv-status").value, tag = val("inv-tag");
    if (status) f.status = status;
    if (tag) f.tag = tag;
    return f;
  }

  private validate(): string | null {
    const fLo = val("inv-f-lo"), fHi = val("inv-f-hi"), t0 = val("inv-t0"), t1 = val("inv-t1");
    if ((fLo === "") !== (fHi === "")) return "set both f lo and f hi (or neither)";
    if ((t0 === "") !== (t1 === "")) return "set both from and to (or neither)";
    return null;
  }

  async load() {
    const problem = this.validate();
    $("inv-info").textContent = problem ?? "";
    if (problem) return;
    await Promise.all([this.candidates.load(), this.confirmed.load()]);
  }
}
