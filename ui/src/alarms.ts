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
