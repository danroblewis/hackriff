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

/** The attention scheduler panel: status/shares/bandit summary/leases, POI+coverage gaps for a
 * requested span, and the (collapsed) bandit arm table. */
export class SchedulerPanel {
  private armsLoaded = false;

  constructor(private client: SchedulerClient) {
    $("sch-form").addEventListener("submit", (e) => { e.preventDefault(); void this.load(); });
    $<HTMLDetailsElement>("sch-arms-details").addEventListener("toggle", () => {
      if ($<HTMLDetailsElement>("sch-arms-details").open && !this.armsLoaded) void this.loadArms();
    });
  }

  private params(): SchedulerParams {
    const v = (id: string) => $<HTMLInputElement>(id).value.trim();
    const fLo = v("sch-f-lo"), fHi = v("sch-f-hi"), t0 = v("sch-t0"), t1 = v("sch-t1");
    const p: SchedulerParams = {};
    if (fLo && fHi) { p.fLoHz = +fLo * 1e6; p.fHiHz = +fHi * 1e6; }
    if (t0 && t1) { p.t0 = +t0; p.t1 = +t1; }
    return p;
  }

  async load() {
    const info = $("sch-info");
    info.textContent = "loading…";
    try {
      const r = await loadScheduler(this.client, this.params());
      this.render(r);
      info.textContent = "";
    } catch (e) {
      info.textContent = errText(e);
    }
  }

  private render(r: SchedulerResponse) {
    const s = r.scheduler;
    const status = $("sch-status");
    if (!s) {
      status.textContent = "no scheduler on this run (interactive hk serve, or a plain replay)";
      $("sch-bandit").textContent = "";
    } else {
      status.textContent = `now ${fmtS(s.now)} · plan v${s.plan_version} · window ${s.window_s}s · ` +
        `sweep floor ${(s.sweep_floor * 100).toFixed(0)}% ${s.sweep_floor_met ? "met" : `NOT MET (${s.floor_violations} violations)`}` +
        `${s.interactive ? " · interactive" : ""}${s.low_power ? " · low power" : ""} · ${sharesText(s.shares_s)}`;
      $("sch-bandit").textContent = s.bandit ? bandit_summaryText(s.bandit) : "bandit off (plan does not enable extra.bandit)";
    }
    $("sch-leases-body").replaceChildren(...r.leases.map((l) => {
      const tr = document.createElement("tr");
      tr.append(td(String(l.id)), td(l.kind), td((l.center_hz / 1e6).toFixed(4), "num"), td((l.rate_hz / 1e6).toFixed(3), "num"),
        td(l.duration_s === null ? "held" : `${l.duration_s}s`, "num"));
      return tr;
    }));
    $("sch-leases-info").textContent = `${r.leases.length} lease${r.leases.length === 1 ? "" : "s"} · observation log ${r.observation_log ? "yes" : "no"}`;

    if (!r.span) {
      $("sch-poi-info").textContent = "set from/to to compute POI and coverage gaps for a span";
      $("sch-poi-body").replaceChildren();
    } else {
      $("sch-poi-info").textContent = `span ${fmtS(r.span.t0)} – ${fmtS(r.span.t1)}${r.poi_truncated ? " (POI truncated)" : ""}`;
      $("sch-poi-body").replaceChildren(...r.poi.map((b) => this.poiRow(b)));
    }
  }

  private poiRow(b: PoiBand): HTMLTableRowElement {
    const tr = document.createElement("tr");
    const poiText = b.poi.map((p) => `τ${p.tau_s}s ${(p.p_poi * 100).toFixed(0)}%`).join(", ");
    tr.append(
      td(`${(b.f_lo / 1e6).toFixed(4)}–${(b.f_hi / 1e6).toFixed(4)} MHz`),
      td(`${(b.observed_fraction * 100).toFixed(1)}%`, "num"),
      td(b.mean_revisit_s !== null ? `${b.mean_revisit_s.toFixed(2)}s` : "—", "num opt"),
      td(poiText, "opt"),
      td(`${b.gaps.length}${b.gaps_truncated ? "+" : ""}`, "num opt"),
    );
    return tr;
  }

  private async loadArms() {
    this.armsLoaded = true;
    const info = $("sch-arms-info");
    info.textContent = "loading…";
    try {
      const r = await loadArms(this.client);
      if (!r.scheduler) { info.textContent = "no scheduler on this run"; $("sch-arms-body").replaceChildren(); return; }
      if (!r.bandit) { info.textContent = "bandit off"; $("sch-arms-body").replaceChildren(); return; }
      $("sch-arms-body").replaceChildren(...r.arms.map((a) => this.armRow(a)));
      info.textContent = `${r.arms.length} arm${r.arms.length === 1 ? "" : "s"}`;
    } catch (e) {
      info.textContent = errText(e);
      this.armsLoaded = false;
    }
  }

  private armRow(a: Arm): HTMLTableRowElement {
    const tr = document.createElement("tr");
    tr.append(
      td(String(a.index), "num"),
      td((a.center_hz / 1e6).toFixed(4), "num"),
      td(a.active ? "active" : "", "opt"),
      td(String(a.visits), "num"),
      td(a.mean_reward.toFixed(3), "num opt"),
      td(typeof a.ucb === "number" ? a.ucb.toFixed(3) : a.ucb, "num opt"),
      td(a.staleness_s.toFixed(1), "num opt"),
      td((a.suspect_fraction * 100).toFixed(0) + "%", "num opt"),
    );
    return tr;
  }
}
