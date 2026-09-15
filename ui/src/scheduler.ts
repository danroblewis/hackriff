// Attention scheduler panel (T-123 over docs/api.md "Attention scheduler", T-127; ADR-0012 §5):
// status, tier shares, bandit summary, leases, POI/coverage gaps and the (collapsed) arm table.
// Thin client: every number here is the server's own; this file only formats already-computed
// figures and builds the query (POI is computed only when a span is given, matching the API's own
// "a bare status poll never scans the observation log" rule). `scheduler: null` (no scheduler on
// this run) is rendered, not treated as an error.

// ---- Wire types (docs/api.md "Attention scheduler") ----

export interface BanditCounters {
  repacks: number; outcomes: number; outcomes_unmatched: number; exploit_dwells: number;
  explore_dwells: number; stale_forced: number; beacon_dwells: number;
  verifications_started: number; verifications_dropped: number; verifications_passed: number; verifications_failed: number;
  floor_deferrals: number; arms_dropped: number; suspect_wasted_s: number;
}

export interface BanditSummary {
  provider_version: number; arms: number; active_arms: number; pending_verifications: number;
  banned: number; total_dwell_s: number; config: unknown; counters: BanditCounters;
}

export interface SchedulerStatus {
  now: number; plan_version: number; window_s: number;
  shares_s: { discovery: number; exploit: number; explore: number; other: number };
  sweep_floor: number; sweep_floor_met: boolean; floor_violations: number;
  interactive: boolean; leases: number; scheduled: number; low_power: boolean;
  bandit: BanditSummary | null;
}

export interface Lease { id: number; kind: string; center_hz: number; rate_hz: number; duration_s: number | null }

export interface PoiGap { f_lo: number; f_hi: number; t0: number; t1: number }
export interface PoiBand {
  f_lo: number; f_hi: number; cell_hz: number; cells: number; observed_cells: number;
  observed_fraction: number; mean_revisit_s: number | null;
  poi: { tau_s: number; p_poi: number; p_poi_min: number }[];
  gap_threshold_s: number; gaps: PoiGap[]; gaps_truncated: boolean;
}

export interface SchedulerResponse {
  scheduler: SchedulerStatus | null;
  leases: Lease[];
  observation_log: boolean;
  span: { t0: number; t1: number } | null;
  poi: PoiBand[];
  poi_truncated: boolean;
}

export interface Arm {
  index: number; key: unknown; center_hz: number; rate_hz: number; active: boolean; exploration: boolean;
  on_dc: boolean; prior: number; mean_reward: number; dwell_s: number; ucb: number | "inf"; visits: number;
  staleness_s: number; suspect_fraction: number; lead: string | null; members: number;
  dwell_planned_s: number; required_revisit_s: number; complete_capture: boolean; last_reward: number | null;
}

export interface ArmsResponse { scheduler: boolean; bandit: boolean; arms: Arm[] }

// ---- Pure helpers: query building, formatting (unit-tested without a DOM) ----

export interface SchedulerParams { fLoHz?: number; fHiHz?: number; t0?: number; t1?: number; tauS?: number[] }

export function schedulerQuery(p: SchedulerParams): string {
  const q = new URLSearchParams();
  if (p.fLoHz !== undefined) q.set("f_lo", String(p.fLoHz));
  if (p.fHiHz !== undefined) q.set("f_hi", String(p.fHiHz));
  if (p.t0 !== undefined) q.set("t0", String(p.t0));
  if (p.t1 !== undefined) q.set("t1", String(p.t1));
  if (p.tauS?.length) q.set("tau_s", p.tauS.join(","));
  const s = q.toString();
  return s ? `/api/scheduler?${s}` : "/api/scheduler";
}

export function fmtS(s: number): string {
  return new Date(s * 1000).toISOString().replace("T", " ").slice(0, 19) + "Z";
}

export function sharesText(s: SchedulerStatus["shares_s"]): string {
  return `discovery ${s.discovery.toFixed(1)}s · exploit ${s.exploit.toFixed(1)}s · explore ${s.explore.toFixed(1)}s · other ${s.other.toFixed(1)}s`;
}

export function bandit_summaryText(b: BanditSummary): string {
  return `${b.active_arms}/${b.arms} active arms · ${b.banned} banned · ${b.pending_verifications} pending verification` +
    `${b.pending_verifications === 1 ? "" : "s"} · ${b.total_dwell_s.toFixed(1)}s total dwell · v${b.provider_version}`;
}

// ---- API calls (thin wrappers so request shapes are unit-tested without a DOM) ----

export interface SchedulerClient { get<T = unknown>(path: string): Promise<T> }

export async function loadScheduler(client: SchedulerClient, p: SchedulerParams): Promise<SchedulerResponse> {
  return client.get<SchedulerResponse>(schedulerQuery(p));
}

export async function loadArms(client: SchedulerClient): Promise<ArmsResponse> {
  return client.get<ArmsResponse>("/api/scheduler/arms");
}
