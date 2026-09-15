// Survey report panel (T-123 over docs/api.md "Survey reports", T-121; ADR-0012 §6). Thin client:
// every occupancy figure, suggestion, change and coverage statement is the server's own; this file
// only builds the query, formats already-computed numbers/labels for display, and links to the
// CSV/PNG exports (`GET /api/report?...&format=csv|png`). No occupancy/novelty/POI math happens
// here.
import { fmtT } from "./history";

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
