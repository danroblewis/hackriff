// Survey report panel (T-123 over docs/api.md "Survey reports", T-121; ADR-0012 §6). Thin client:
// every occupancy figure, suggestion, change and coverage statement is the server's own; this file
// only builds the query, formats already-computed numbers/labels for display, and links to the
// CSV/PNG exports (`GET /api/report?...&format=csv|png`). No occupancy/novelty/POI math happens
// here.
import { fmtT, fromUtcInput, utcInput } from "./history";
import type { HistoryPanel } from "./history";

// ---- Wire types (docs/api.md "Survey reports"; `hk_model::attention::report::SurveyReport") ----

export interface FreqRange { lo_hz: number; hi_hz: number }
export interface TimeRangeNs { start: number; end: number } // Timestamp = Unix nanoseconds

export interface OccupancyRow {
  schema?: number;
  subject: unknown;
  interval?: TimeRangeNs;
  fco: number | null;
  fco_all_visits: number | null;
  fbo?: number | null;
  n_revisits?: number;
  n_occupied?: number;
  n_revisits_all?: number;
  observed_s?: number;
  revisit_biased?: boolean;
  threshold?: { method?: { method?: string } };
  subject_extent?: FreqRange;
}

export interface TopEmitter {
  emitter_id: string;
  freq: FreqRange;
  first_seen: number; // Timestamp ns
  last_seen: number; // Timestamp ns
  sightings: number;
  lifecycle: "candidate" | "confirmed";
  fco: number | null;
  fco_all_visits: number | null;
  top_suggestion: string | null;
  new_in_span: boolean;
}

export type ChangeStatus = "available" | "immature" | "no-baseline" | "unavailable";

export interface ChangeEntry {
  subject: unknown;
  kind: "level-above-baseline" | "busier-than-usual" | "quieter-than-usual" | string;
  baseline: number;
  observed: number;
  z: number;
}

export interface ChangeVsBaseline {
  status: ChangeStatus;
  baseline?: unknown;
  resolution?: string;
  changes: ChangeEntry[];
}

export interface CoverageGap { freq: FreqRange; time: TimeRangeNs }
export interface PoiRow { tau_s: number; p_poi: number }

export interface ReportCoverage {
  observed_fraction: number;
  observed_s: number;
  gaps: CoverageGap[];
  gaps_truncated: boolean;
  never_observed: FreqRange[];
  poi: PoiRow[];
  statement: string;
}

export interface ProvenanceStep { t: number; kind: string; freq?: FreqRange; detail: string }

export interface SurveyReport {
  schema: number;
  generated_at: number; // Timestamp ns
  region: FreqRange;
  span: TimeRangeNs;
  site: unknown;
  occupancy: { bands: OccupancyRow[]; channels: OccupancyRow[]; truncated: boolean };
  top_emitters: TopEmitter[];
  change_vs_baseline: ChangeVsBaseline;
  coverage: ReportCoverage;
  provenance_steps: ProvenanceStep[];
  anomalies: unknown[];
  warnings: string[];
}

// ---- Pure helpers (query building, formatting): unit-tested without a DOM ----

export interface ReportParams { fLoHz: number; fHiHz: number; t0: number; t1: number; site?: string }

/** `GET /api/report` query string for a region/span (JSON format; CSV/PNG add `&format=`). */
export function reportQuery(p: ReportParams): string {
  const q = new URLSearchParams();
  q.set("f_lo", String(p.fLoHz));
  q.set("f_hi", String(p.fHiHz));
  q.set("t0", String(p.t0));
  q.set("t1", String(p.t1));
  if (p.site) q.set("site", p.site);
  return `/api/report?${q}`;
}

/** The same query with a `format` for the CSV/PNG export links. */
export function reportExportUrl(p: ReportParams, format: "csv" | "png"): string {
  return `${reportQuery(p)}&format=${format}`;
}

/** Occupancy fraction text: `fco` when the source can give the unbiased figure, else
 * `fco_all_visits` labelled "indicative (biased)" (docs/api.md: `fco_all_visits` is information
 * only and never replaces `fco`) — the UI shows the API's own distinction, it does not compute it. */
export function fcoText(row: Pick<OccupancyRow, "fco" | "fco_all_visits">): string {
  if (row.fco !== null && row.fco !== undefined) return `${(row.fco * 100).toFixed(1)}%`;
  if (row.fco_all_visits !== null && row.fco_all_visits !== undefined) return `${(row.fco_all_visits * 100).toFixed(1)}% (indicative, biased)`;
  return "—";
}

export function freqText(f: FreqRange | undefined): string {
  if (!f) return "—";
  return `${(f.lo_hz / 1e6).toFixed(4)}–${(f.hi_hz / 1e6).toFixed(4)} MHz`;
}

export function fmtNs(ns: number | undefined): string {
  return ns === undefined || !Number.isFinite(ns) ? "—" : fmtT(ns / 1e9);
}

const STATUS_TEXT: Record<ChangeStatus, string> = {
  available: "available",
  immature: "immature (baseline not mature yet)",
  "no-baseline": "no baseline (mobile/unassigned site, or no baselined subject)",
  unavailable: "unavailable (this server has no baselines)",
};

export function changeStatusText(status: ChangeStatus): string {
  return STATUS_TEXT[status] ?? status;
}

// ---- API call (thin wrapper so the request/response shape is unit-tested without a DOM) ----

export interface ReportClient { get<T = unknown>(path: string): Promise<T> }

export async function loadReport(client: ReportClient, p: ReportParams): Promise<SurveyReport> {
  return client.get<SurveyReport>(reportQuery(p));
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

/** Survey report panel: region/span form over `GET /api/report`, rendering occupancy, top
 * emitters, change-vs-baseline, coverage/POI and provenance from the response only. */
export class ReportPanel {
  private last: ReportParams | null = null;

  constructor(private client: ReportClient, private history: HistoryPanel, private token: string) {
    $("rpt-form").addEventListener("submit", (e) => { e.preventDefault(); void this.load(); });
    $("rpt-use-region").addEventListener("click", () => this.useHistoryRegion());
  }

  /** Copies the region-over-time frequency and time window into the report form (reuses
   * `HistoryPanel.region()`, the same helper the inventory panel copies from). */
  private useHistoryRegion() {
    const r = this.history.region();
    if (Number.isFinite(r.fLoHz) && Number.isFinite(r.fHiHz)) {
      $<HTMLInputElement>("rpt-f-lo").value = (r.fLoHz / 1e6).toFixed(6);
      $<HTMLInputElement>("rpt-f-hi").value = (r.fHiHz / 1e6).toFixed(6);
    }
    if (Number.isFinite(r.t0) && Number.isFinite(r.t1)) {
      $<HTMLInputElement>("rpt-t0").value = utcInput(r.t0);
      $<HTMLInputElement>("rpt-t1").value = utcInput(r.t1);
    }
  }

  private params(): ReportParams | null {
    const v = (id: string) => $<HTMLInputElement>(id).value.trim();
    const fLo = v("rpt-f-lo"), fHi = v("rpt-f-hi"), t0 = v("rpt-t0"), t1 = v("rpt-t1");
    if (!fLo || !fHi || !t0 || !t1) return null;
    const site = v("rpt-site");
    return { fLoHz: +fLo * 1e6, fHiHz: +fHi * 1e6, t0: fromUtcInput(t0), t1: fromUtcInput(t1), site: site || undefined };
  }

  async load() {
    const p = this.params();
    const info = $("rpt-info");
    if (!p) { info.textContent = "set f lo, f hi, from and to"; return; }
    this.last = p;
    info.textContent = "loading…";
    $("rpt-body").hidden = true;
    this.setExportLinks(p);
    try {
      const r = await loadReport(this.client, p);
      info.textContent = `generated ${fmtNs(r.generated_at)} · ${freqText(r.region)}`;
      $("rpt-body").hidden = false;
      this.renderCoverage(r.coverage);
      this.renderOccupancy(r.occupancy);
      this.renderEmitters(r.top_emitters);
      this.renderChanges(r.change_vs_baseline);
      this.renderProvenance(r.provenance_steps);
      $("rpt-warnings").replaceChildren(...r.warnings.map((w) => { const li = document.createElement("li"); li.textContent = w; return li; }));
      $("rpt-warnings-wrap").hidden = r.warnings.length === 0;
    } catch (e) {
      info.textContent = errText(e);
    }
  }

  private setExportLinks(p: ReportParams) {
    $<HTMLAnchorElement>("rpt-csv").href = `${reportExportUrl(p, "csv")}&token=${encodeURIComponent(this.token)}`;
    $<HTMLAnchorElement>("rpt-png").href = `${reportExportUrl(p, "png")}&token=${encodeURIComponent(this.token)}`;
    $("rpt-exports").hidden = false;
  }

  /** Coverage is shown prominently, above the occupancy detail (CLAUDE.md/docs/api.md: unobserved
   * is never reported as quiet). */
  private renderCoverage(c: ReportCoverage) {
    $("rpt-coverage-statement").textContent = c.statement;
    $("rpt-coverage-stats").textContent =
      `observed ${(c.observed_fraction * 100).toFixed(1)}% · ${c.observed_s.toFixed(1)} s observed` +
      (c.gaps.length ? ` · ${c.gaps.length} gap${c.gaps.length === 1 ? "" : "s"}${c.gaps_truncated ? " (truncated)" : ""}` : "") +
      (c.never_observed.length ? ` · ${c.never_observed.length} never-observed range${c.never_observed.length === 1 ? "" : "s"}` : "");
    $("rpt-poi-body").replaceChildren(...c.poi.map((p) => {
      const tr = document.createElement("tr");
      tr.append(td(`${p.tau_s} s`, "num"), td((p.p_poi * 100).toFixed(1) + "%", "num"));
      return tr;
    }));
    $("rpt-gaps-body").replaceChildren(...c.gaps.map((g) => {
      const tr = document.createElement("tr");
      tr.append(td(freqText(g.freq)), td(`${fmtNs(g.time.start)} – ${fmtNs(g.time.end)}`));
      return tr;
    }));
    $("rpt-never-body").replaceChildren(...c.never_observed.map((f) => {
      const tr = document.createElement("tr");
      tr.append(td(freqText(f)));
      return tr;
    }));
  }

  private renderOccupancy(o: SurveyReport["occupancy"]) {
    const rows = (rs: OccupancyRow[]) => rs.map((r) => {
      const tr = document.createElement("tr");
      tr.append(
        td(freqText(r.subject_extent)),
        td(fcoText(r), "num"),
        td(r.fbo !== null && r.fbo !== undefined ? `${(r.fbo * 100).toFixed(1)}%` : "—", "num"),
        td(r.n_revisits !== undefined ? String(r.n_revisits) : "—", "num opt"),
        td(r.observed_s !== undefined ? r.observed_s.toFixed(1) : "—", "num opt"),
        td(r.revisit_biased ? "biased" : "", "opt"),
      );
      return tr;
    });
    $("rpt-bands-body").replaceChildren(...rows(o.bands));
    $("rpt-channels-body").replaceChildren(...rows(o.channels));
    $("rpt-occ-info").textContent = o.truncated ? "channel list truncated to 64 rows" : "";
  }

  private renderEmitters(list: TopEmitter[]) {
    $("rpt-emitters-body").replaceChildren(...list.map((e) => {
      const tr = document.createElement("tr");
      tr.append(
        td(e.emitter_id),
        td(freqText(e.freq)),
        td(e.lifecycle),
        td(String(e.sightings), "num"),
        td(fcoText(e), "num"),
        td(e.new_in_span ? "new" : "", "opt"),
      );
      const sug = document.createElement("td");
      sug.textContent = e.top_suggestion ? `${e.top_suggestion} (suggestion)` : "—";
      tr.append(sug);
      return tr;
    }));
    $("rpt-emitters-info").textContent = `${list.length} emitter${list.length === 1 ? "" : "s"}`;
  }

  private renderChanges(cvb: ChangeVsBaseline) {
    $("rpt-change-status").textContent = changeStatusText(cvb.status);
    $("rpt-change-body").replaceChildren(...cvb.changes.map((c) => {
      const tr = document.createElement("tr");
      tr.append(td(c.kind), td(c.baseline.toFixed(2), "num"), td(c.observed.toFixed(2), "num"), td(c.z.toFixed(2), "num"));
      return tr;
    }));
    $("rpt-change-table").hidden = cvb.changes.length === 0;
  }

  private renderProvenance(steps: ProvenanceStep[]) {
    $("rpt-prov-body").replaceChildren(...steps.map((s) => {
      const tr = document.createElement("tr");
      tr.append(td(fmtNs(s.t)), td(s.kind), td(s.freq ? freqText(s.freq) : "—", "opt"), td(s.detail));
      return tr;
    }));
  }
}
