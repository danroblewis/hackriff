// Novelty alarm list (T-123 over docs/api.md "Anomalies and novelty alarms", T-122; ADR-0012
// §7-§8). Thin client: kind/status/explanations/history are the server's own; this file only
// builds queries, formats already-ranked explanations for display (including `unexplained`, which
// stays visible rather than being hidden), and sends dismiss/reopen with the auth token like every
// other mutating call (`controls/client.ts` `ControlClient`).

// ---- Wire types (docs/api.md "Anomalies and novelty alarms"; times are Unix seconds) ----

export type AnomalyKind =
  | "new-emitter" | "busier-than-baseline" | "quieter-than-baseline"
  | "noise-floor-rise" | "novelty" | "level-above-baseline" | "change-point";
export type AnomalyStatus = "open" | "resolved" | "dismissed";
export type AlarmState = "open" | "cleared" | "dismissed" | "explained";

export interface ExplanationCause {
  kind: "external-event" | "emitter" | "own-history" | "self-inflicted" | "unexplained";
  reason?: string;
}

export interface Explanation {
  id: string;
  cause: ExplanationCause;
  correlation_type?: string;
  score: number;
  provisional: boolean;
  rule_version?: number;
  t: number;
  evidence?: unknown;
}

export interface AlarmDetail {
  key: { kind: string; site: unknown; subject: unknown };
  state: AlarmState;
  last_transition: string;
  raised_at: number;
  last_t: number;
  reopen_count: number;
  cleared_at?: number;
  dismissed_until?: number;
  f_lo: number;
  f_hi: number;
  detail: unknown;
  explained_step_t?: number;
}

export interface AnomalyRow {
  id: string;
  kind: AnomalyKind;
  subject: unknown;
  f_lo: number;
  f_hi: number;
  t0: number;
  t1: number;
  t: number;
  score: number;
  baseline_ref?: unknown;
  detector_version?: number;
  status: AnomalyStatus;
  alarm: AlarmDetail | null;
  explanations: Explanation[];
}

export interface HistoryEntry { status: string; t: number; note: string }

export interface AnomalyDetailDto extends AnomalyRow { history: HistoryEntry[] }

export interface AnomaliesPage {
  anomalies: AnomalyRow[];
  next_cursor: string | null;
  truncated: boolean;
  suppressions: Record<string, Record<string, number>>;
}

// ---- Pure helpers: query building, request bodies, formatting (unit-tested without a DOM) ----

export interface AnomalyFilters { fLoHz?: number; fHiHz?: number; t0?: number; t1?: number; kind?: string; status?: string }

export function anomaliesQuery(f: AnomalyFilters, cursor?: string | null, limit = 100): string {
  const q = new URLSearchParams();
  if (f.fLoHz !== undefined) q.set("f_lo", String(f.fLoHz));
  if (f.fHiHz !== undefined) q.set("f_hi", String(f.fHiHz));
  if (f.t0 !== undefined) q.set("t0", String(f.t0));
  if (f.t1 !== undefined) q.set("t1", String(f.t1));
  if (f.kind) q.set("kind", f.kind);
  if (f.status) q.set("status", f.status);
  q.set("limit", String(limit));
  if (cursor) q.set("cursor", cursor);
  return `/api/anomalies?${q}`;
}

/** The top-ranked explanation's label, `unexplained` shown plainly like every other cause. */
export function topExplanationText(explanations: readonly Explanation[]): string {
  if (!explanations.length) return "—";
  const top = explanations[0];
  const label = top.cause.kind === "self-inflicted" && top.cause.reason ? `self-inflicted: ${top.cause.reason}` : top.cause.kind;
  return `${label} (${(top.score * 100).toFixed(0)}%)`;
}

export function fmtS(s: number | undefined): string {
  return s === undefined || !Number.isFinite(s) ? "—" : new Date(s * 1000).toISOString().replace("T", " ").slice(0, 19) + "Z";
}

export function freqRangeText(fLoHz: number, fHiHz: number): string {
  return `${(fLoHz / 1e6).toFixed(4)}–${(fHiHz / 1e6).toFixed(4)} MHz`;
}

/** Dismiss is refused (409) on a floor episode (no `alarm`) or a self-inflicted anomaly (docs/api.md). */
export function canDismiss(a: Pick<AnomalyRow, "status" | "alarm">): boolean {
  return a.alarm !== null && a.status === "open" && a.alarm.state !== "explained";
}

export function canReopen(a: Pick<AnomalyRow, "status" | "alarm">): boolean {
  return a.alarm !== null && (a.alarm.state === "dismissed" || a.alarm.state === "cleared");
}

// ---- API calls (thin wrappers so request shapes are unit-tested without a DOM) ----

export interface AnomaliesClient {
  get<T = unknown>(path: string): Promise<T>;
  post<T = unknown>(path: string, body?: unknown): Promise<T>;
}

export async function loadAnomalies(client: AnomaliesClient, f: AnomalyFilters, cursor?: string | null, limit?: number): Promise<AnomaliesPage> {
  return client.get<AnomaliesPage>(anomaliesQuery(f, cursor, limit));
}

export async function loadAnomalyDetail(client: AnomaliesClient, id: string): Promise<AnomalyDetailDto> {
  return client.get<AnomalyDetailDto>(`/api/anomalies/${encodeURIComponent(id)}`);
}

export async function dismissAnomaly(client: AnomaliesClient, id: string, note?: string): Promise<AnomalyRow> {
  return client.post<AnomalyRow>(`/api/anomalies/${encodeURIComponent(id)}/dismiss`, note ? { note } : {});
}

export async function reopenAnomaly(client: AnomaliesClient, id: string): Promise<AnomalyRow> {
  return client.post<AnomalyRow>(`/api/anomalies/${encodeURIComponent(id)}/reopen`, {});
}

// ---- DOM wiring (untested under node:test, like the rest of ui/src's panels; only the pure
// functions above are imported by tests) ----

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function errText(e: unknown): string {
  const anyE = e as { code?: unknown };
  const code = anyE && typeof anyE.code === "string" ? anyE.code : null;
  const msg = e instanceof Error ? e.message : String(e);
  return code ? `${msg} (${code})` : msg;
}

function td(text: string, cls = ""): HTMLTableCellElement {
  const c = document.createElement("td");
  c.textContent = text;
  if (cls) c.className = cls;
  return c;
}

/** The novelty alarm list panel: filters, paging, row -> detail with history and dismiss/reopen. */
export class AlarmsPanel {
  private rows: AnomalyRow[] = [];
  private cursor: string | null = null;
  private selected: string | null = null;

  constructor(private client: AnomaliesClient) {
    $("al-form").addEventListener("submit", (e) => { e.preventDefault(); void this.load(); });
    $("al-more").addEventListener("click", () => void this.load(true));
    $("al-detail-close").addEventListener("click", () => this.closeDetail());
  }

  private filters(): AnomalyFilters {
    const kind = $<HTMLSelectElement>("al-kind").value, status = $<HTMLSelectElement>("al-status").value;
    return { kind: kind || undefined, status: status || undefined };
  }

  async load(more = false) {
    const info = $("al-info");
    if (!more) { this.rows = []; this.cursor = null; this.closeDetail(); }
    info.textContent = "loading…";
    try {
      const page = await loadAnomalies(this.client, this.filters(), more ? this.cursor : null);
      this.rows.push(...page.anomalies);
      this.cursor = page.next_cursor;
      this.render();
      const sup = Object.entries(page.suppressions).flatMap(([k, v]) => Object.entries(v).map(([r, n]) => `${k}/${r}: ${n}`));
      info.textContent = `${this.rows.length} loaded${this.cursor ? " (more on the server)" : ""}${sup.length ? ` · suppressed: ${sup.join(", ")}` : ""}`;
      $("al-more").hidden = !this.cursor;
    } catch (e) {
      info.textContent = errText(e);
      $("al-more").hidden = true;
    }
  }

  private render() {
    $("al-body").replaceChildren(...this.rows.map((a) => this.tr(a)));
  }

  private tr(a: AnomalyRow): HTMLTableRowElement {
    const tr = document.createElement("tr");
    tr.append(
      td(a.kind),
      td(freqRangeText(a.f_lo, a.f_hi)),
      td(a.status),
      td(fmtS(a.t)),
      td(topExplanationText(a.explanations)),
    );
    if (a.id === this.selected) tr.className = "sel";
    tr.addEventListener("click", () => void this.openDetail(a.id));
    return tr;
  }

  private async openDetail(id: string) {
    this.selected = id;
    this.render();
    const box = $("al-detail");
    box.hidden = false;
    $("al-detail-body").textContent = "loading…";
    try {
      const a = await loadAnomalyDetail(this.client, id);
      this.renderDetail(a);
    } catch (e) {
      $("al-detail-body").textContent = errText(e);
    }
  }

  private renderDetail(a: AnomalyDetailDto) {
    const box = $("al-detail-body");
    box.replaceChildren();
    const head = document.createElement("p");
    head.textContent = `${a.kind} · ${freqRangeText(a.f_lo, a.f_hi)} · ${fmtS(a.t0)} – ${fmtS(a.t1)} · status ${a.status}` +
      (a.alarm ? ` · alarm ${a.alarm.state}` : "");
    box.append(head);

    const expl = document.createElement("ul");
    for (const e of a.explanations) {
      const li = document.createElement("li");
      const label = e.cause.kind === "self-inflicted" && e.cause.reason ? `self-inflicted: ${e.cause.reason}` : e.cause.kind;
      li.textContent = `${label} — ${(e.score * 100).toFixed(0)}%${e.provisional ? " (provisional)" : ""}`;
      expl.append(li);
    }
    box.append(expl);

    if (a.history.length) {
      const h = document.createElement("table");
      h.className = "grid";
      const body = document.createElement("tbody");
      for (const h1 of a.history) {
        const tr = document.createElement("tr");
        tr.append(td(fmtS(h1.t)), td(h1.status), td(h1.note));
        body.append(tr);
      }
      h.append(body);
      box.append(h);
    }

    const acts = document.createElement("div");
    acts.className = "acts";
    if (canDismiss(a)) {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = "Dismiss";
      b.addEventListener("click", () => void this.dismiss(a.id));
      acts.append(b);
    }
    if (canReopen(a)) {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = "Reopen";
      b.addEventListener("click", () => void this.reopen(a.id));
      acts.append(b);
    }
    box.append(acts);
  }

  private async dismiss(id: string) {
    try {
      await dismissAnomaly(this.client, id);
      await this.load();
      await this.openDetail(id);
    } catch (e) {
      $("al-detail-body").append(Object.assign(document.createElement("p"), { className: "hint bad", textContent: `dismiss: ${errText(e)}` }));
    }
  }

  private async reopen(id: string) {
    try {
      await reopenAnomaly(this.client, id);
      await this.load();
      await this.openDetail(id);
    } catch (e) {
      $("al-detail-body").append(Object.assign(document.createElement("p"), { className: "hint bad", textContent: `reopen: ${errText(e)}` }));
    }
  }

  private closeDetail() {
    this.selected = null;
    $("al-detail").hidden = true;
    this.render();
  }
}
