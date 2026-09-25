// T-814 (MAP-14): the Explore drawer — places to go, listed in the bottom sheet (docs/23 §5,
// ui/mockups/map-ui-v1.html `renderExplore`). Four groups: unknown & unexplained (the priority),
// strongest right now, quiet-but-active bands, and past-survey windows (the log's dwell windows AND
// its survey sweep passes — T-906 — each Go restoring the survey's time window and frequency span).
//
// THIN CLIENT: every figure is the backend's own (GET /api/events, /api/analysis/strongest,
// /api/scheduler POI, /api/coverage); this file only orders, formats and turns clicks into store
// writes. It issues GETs only and never a device route, so opening, scrolling or clicking the drawer
// cannot move the radio; a frequency outside the tuned window is reached by the surface's own retune
// *offer*, not from here.
//
// docs/23 §10.6 P4 (size inversely proportional to influence): the drawer is a big panel, so a bare
// click / Enter on a row only SELECTS it — highlights the row and, for an emitter, focuses its box on
// the map (`focusSignal`, view state). It never pans, zooms or jumps the view. Jumping is the small,
// explicit per-row "go to" button, which writes `requestGoto` (and `reviewAt` for a past window).
import type { MountFn } from "../context";
import { requestGoto } from "../shell-slice";
import { reviewAt } from "../centre/capture-slice";
import { focusSignal } from "../explore/slice";
import type { AppState } from "../state";
import { schedulerQuery, type SchedulerResponse } from "../../scheduler";

export type DrawerGroup = "unknown" | "strongest" | "quiet" | "surveys";
export interface DrawerItem {
  group: DrawerGroup; tag: string; title: string; why: string;
  /** Where the row's "go to" button goes: a frequency and, for a past window, the time range. */
  hz: number; time?: { t0: number; t1: number };
  /** The frequency span a region row covers (T-906): Go restores it with the centre, so a past
   * survey comes back as the whole (time × frequency) extent it covered, not just its midpoint. */
  spanHz?: number;
  /** The emitter this row is, if any: a row-body click focuses (highlights) its box on the map. */
  emitterId?: string;
  /** A statement, not a place (e.g. "older surveys not loaded"): no go-to button. */
  note?: true;
}

/** A row's stable identity across refreshes, so a selection survives the 30 s re-list. */
export const itemKey = (it: DrawerItem): string => it.emitterId ?? `${it.group}:${it.hz}:${it.time?.t0 ?? ""}`;

/** What a bare row click / Enter writes: selection only (view state), never a view move (P4). */
export const selectItem = (it: DrawerItem) => (s: AppState): Partial<AppState> =>
  it.emitterId !== undefined ? focusSignal(it.emitterId)(s) : {};

/** What the small per-row "go to" button writes: view arithmetic only, never a device route. */
export const gotoItem = (it: DrawerItem) => (s: AppState): Partial<AppState> => ({
  ...(it.time ? reviewAt(it.time.t1, it.time.t1 - it.time.t0)() : {}),
  ...requestGoto(it.hz, it.spanHz)(s),
});
export const GROUP_TITLE: Record<DrawerGroup, string> = {
  unknown: "Unknown & unexplained — the priority", strongest: "Strongest right now",
  quiet: "Quiet but active", surveys: "Past surveys — jump to a coverage window",
};
export const GROUP_ORDER: readonly DrawerGroup[] = ["unknown", "strongest", "quiet", "surveys"];

export const fmtHz = (hz: number): string =>
  hz >= 1e9 ? `${(hz / 1e9).toFixed(3)} GHz` : hz >= 1e6 ? `${(hz / 1e6).toFixed(3)} MHz` : `${(hz / 1e3).toFixed(1)} kHz`;

// ---- wire shapes (docs/api.md), only the fields read ----
export interface EventsResp {
  events: { emitter_id: string; t_start_s: number; t_end_s: number | null; open: boolean; count: number; f_center_hz?: number | null }[];
  emitters: { id: string; state: string; f_center_hz: number; bandwidth_hz: number; known_status: string; explanations: { label: string }[] }[];
}
export interface StrongestResp { found: boolean; f_center_hz?: number; max_db?: number }
export interface CoverageResp {
  window: { t0_s: number; t1_s: number };
  grid: { cells: number; f_lo_hz: number; f_cell_hz: number };
  any?: { cells: { state: string }[] };
}

export function unknownItems(r: EventsResp | null, max = 4): DrawerItem[] {
  if (!r) return [];
  const byId = new Map(r.emitters.map((e) => [e.id, e]));
  const out: DrawerItem[] = [];
  const seen = new Set<string>();
  for (const ev of [...r.events].sort((a, b) => b.t_start_s - a.t_start_s)) {
    const e = byId.get(ev.emitter_id);
    if (!e || seen.has(e.id) || e.known_status !== "unknown") continue;
    seen.add(e.id);
    const hint = e.explanations[0]?.label;
    out.push({
      group: "unknown", tag: ev.open ? "unknown · on air" : ev.count > 1 ? `unknown · burst ×${ev.count}` : "unknown · ended",
      title: `${fmtHz(e.f_center_hz)} · ${fmtHz(e.bandwidth_hz)}`,
      why: hint ? `nothing matched; nearest suggestion: ${hint}` : "no explanation matched — measured, not looked up",
      hz: e.f_center_hz, emitterId: e.id,
    });
    if (out.length >= max) break;
  }
  return out;
}

export function strongestItem(r: StrongestResp | null): DrawerItem[] {
  if (!r?.found || r.f_center_hz === undefined) return [];
  return [{ group: "strongest", tag: "strongest", title: fmtHz(r.f_center_hz),
    why: r.max_db === undefined ? "max-hold over the recent window" : `${r.max_db.toFixed(1)} dB/Hz max-hold, recent window`, hz: r.f_center_hz }];
}

export function quietItems(r: SchedulerResponse | null, max = 4): DrawerItem[] {
  if (!r) return [];
  return r.poi.filter((b) => b.observed_cells > 0 && (b.poi[0]?.p_poi ?? 0) > 0)
    .sort((a, b) => (b.poi[0]?.p_poi ?? 0) - (a.poi[0]?.p_poi ?? 0)).slice(0, max)
    .map((b) => ({
      group: "quiet" as const, tag: "quiet · active",
      title: `${fmtHz(b.f_lo)} – ${fmtHz(b.f_hi)}`,
      why: `scheduler: ${(100 * (b.poi[0]?.p_poi ?? 0)).toFixed(0)} % chance of activity worth a look; ${(100 * b.observed_fraction).toFixed(0)} % observed`,
      hz: (b.f_lo + b.f_hi) / 2,
    }));
}

/** Contiguous runs of observed cells are the survey's extent — the shape of what is not grey. */
export function surveyItems(r: CoverageResp | null, max = 4): DrawerItem[] {
  const cells = r?.any?.cells;
  if (!r || !cells) return [];
  const out: DrawerItem[] = [];
  let start = -1;
  const flush = (end: number) => {
    if (start < 0) return;
    const lo = r.grid.f_lo_hz + start * r.grid.f_cell_hz, hi = r.grid.f_lo_hz + end * r.grid.f_cell_hz;
    out.push({ group: "surveys", tag: "survey · observed", title: `${fmtHz(lo)} – ${fmtHz(hi)}`,
      why: "sampled in the capture window; everything outside is grey (unobserved)", hz: (lo + hi) / 2, spanHz: hi - lo,
      time: { t0: r.window.t0_s, t1: r.window.t1_s } });
    start = -1;
  };
  cells.forEach((c, i) => { if (c.state === "observed" || c.state === "excluded") { if (start < 0) start = i; } else flush(i); });
  flush(cells.length);
  return out.slice(0, max);
}

// ---- T-815: past surveys from the observation log (docs/api.md GET /api/observations) ----
export interface ObservationsResp {
  records: { record: string; window?: { usable: { lo_hz: number; hi_hz: number } };
    observed?: { start_ns: number; end_ns: number };
    /** Sweep records (T-906): the pass, its geometry id and the hops it actually visited. */
    survey_id?: string; geometry?: number; span?: { start_ns: number; end_ns: number };
    visits?: { hop: number; observed_ms: number }[] }[];
  geometries?: { id: number; hops: { usable: { lo_hz: number; hi_hz: number } }[] }[];
  next_cursor?: number | null;
}
type Get = <T>(path: string) => Promise<T | null>;

/** One observed-then window: a band and the time the log says it was looked at. `sweep` names the
 * survey pass a window came from (T-906); a dwell window has none. */
export interface SurveyWindow { lo: number; hi: number; t0: number; t1: number; sweep?: string }

/**
 * Fold observation records into windows. Idempotent: folding a record twice (overlapping reads)
 * changes nothing.
 *
 * - **Dwell** records: one band, merged with the same band when their times touch (within `joinS`).
 * - **Sweep** records (T-906 decision): a survey sweep IS a past survey, so it is listed — one row
 *   per pass (`survey_id`), spanning the hops the log says were actually visited (`observed_ms > 0`)
 *   and the pass's time. A sweep hears each hop only during its own visit, and the row says so; the
 *   map's grey inside that extent stays the honest per-cell answer. A sweep record whose geometry
 *   the page did not carry has no band this client can state, and is skipped rather than guessed.
 */
export function foldSurveyRecords(wins: SurveyWindow[], records: ObservationsResp["records"], joinS = 120,
  geometries: ObservationsResp["geometries"] = []): SurveyWindow[] {
  for (const rec of records) {
    if (rec.record === "sweep") { foldSweep(wins, rec, geometries ?? [], joinS); continue; }
    if (rec.record !== "dwell" || !rec.window || !rec.observed) continue;
    const t0 = rec.observed.start_ns / 1e9, t1 = rec.observed.end_ns / 1e9;
    if (!(t1 > t0)) continue;
    const { lo_hz: lo, hi_hz: hi } = rec.window.usable;
    const m = wins.find((w) => w.sweep === undefined && Math.abs(w.lo - lo) < 1 && Math.abs(w.hi - hi) < 1 && t0 <= w.t1 + joinS && t1 >= w.t0 - joinS);
    if (m) { m.t0 = Math.min(m.t0, t0); m.t1 = Math.max(m.t1, t1); } else wins.push({ lo, hi, t0, t1 });
  }
  return wins;
}

function foldSweep(wins: SurveyWindow[], rec: ObservationsResp["records"][number],
  geometries: NonNullable<ObservationsResp["geometries"]>, joinS: number): void {
  if (!rec.span || rec.geometry === undefined || !rec.visits) return;
  const geo = geometries.find((g) => g.id === rec.geometry);
  if (!geo) return;
  let lo = Infinity, hi = -Infinity;
  for (const v of rec.visits) {
    const h = geo.hops[v.hop];
    if (!h || !(v.observed_ms > 0)) continue;
    lo = Math.min(lo, h.usable.lo_hz); hi = Math.max(hi, h.usable.hi_hz);
  }
  const t0 = rec.span.start_ns / 1e9, t1 = rec.span.end_ns / 1e9;
  if (!(hi > lo) || !(t1 > t0)) return;
  const sweep = rec.survey_id ?? "";
  const m = wins.find((w) => w.sweep === sweep && t0 <= w.t1 + joinS && t1 >= w.t0 - joinS);
  if (m) {
    m.lo = Math.min(m.lo, lo); m.hi = Math.max(m.hi, hi); m.t0 = Math.min(m.t0, t0); m.t1 = Math.max(m.t1, t1);
  } else wins.push({ lo, hi, t0, t1, sweep });
}

/** The documented maximum page (docs/api.md: `limit` at most 10000). */
export const OBS_PAGE_LIMIT = 10_000;
export const SURVEY_LOOKBACK_S = 7 * 86400;
/** The log is served oldest-first with no newest-first order, so it is read in time slices from the
 * live edge backwards: each slice is paged to its end before the next-older one starts. */
export const SURVEY_SLICE_S = 86400;
/** Re-reads start this far before the last read's edge, so a record sealed late is not missed. */
export const SURVEY_REREAD_S = 120;

/**
 * The past-survey windows of the last 7 days, kept across refreshes.
 *
 * Correctness (T-815 review): the route returns records oldest first in log order, and a scheduled
 * scan writes ~8,600 dwell records a day, so reading from the start of a 7-day box and stopping at a
 * page cap listed days-old surveys as "newest". Here the newest slice is always read first and in
 * full; a page budget exists only as a guard against a pathological log, and when it binds the
 * drawer says so (`truncatedBefore`) instead of silently mislabelling what it did read.
 *
 * Cost: after the first load a refresh reads only `[last edge − SURVEY_REREAD_S, edge]`, not the
 * whole window again.
 */
export class SurveyLog {
  wins: SurveyWindow[] = [];
  /** The live edge the log has been read up to, or null before the first successful read. */
  readThrough: number | null = null;
  /** Set when the page budget bound: surveys that ended before this time may be missing. */
  truncatedBefore: number | null = null;
  /** Set when an older slice FAILED to load (network), as opposed to the page budget binding: the
   * range `[floor, retryBefore]` is read again on the next refresh (T-906), so a transient failure
   * does not leave "not fully loaded" standing until that day ages out of the window. */
  retryBefore: number | null = null;
  private busy: Promise<void> | null = null;

  constructor(private readonly maxPages = 40, private readonly pageLimit = OBS_PAGE_LIMIT) {}

  refresh(get: Get, edge: number): Promise<void> {
    this.busy ??= this.read(get, edge).finally(() => { this.busy = null; });
    return this.busy;
  }

  private async read(get: Get, edge: number): Promise<void> {
    if (this.readThrough !== null && edge < this.readThrough - SURVEY_REREAD_S) this.reset(); // a new run / clock
    const floor = edge - SURVEY_LOOKBACK_S;
    const from = this.readThrough === null ? floor : Math.max(floor, this.readThrough - SURVEY_REREAD_S);
    const budget = { pages: 0 };
    // Older slices a PREVIOUS refresh failed to fetch are read again after the newest data (never
    // within the refresh that failed on them). Folding is idempotent, so a re-read changes nothing.
    const retry = this.retryBefore, trunc0 = this.truncatedBefore;
    this.retryBefore = null;
    if (edge > from && !(await this.readRange(get, from, edge, true, budget))) { this.retryBefore = retry; return; }
    if (retry !== null && retry > floor) {
      if (this.truncatedBefore === trunc0 && trunc0 !== null && trunc0 <= retry) this.truncatedBefore = null;
      await this.readRange(get, floor, Math.min(retry, from), false, budget);
    }
    this.wins = this.wins.filter((w) => w.t1 >= floor);
    if (this.truncatedBefore !== null && this.truncatedBefore < floor) this.truncatedBefore = null;
    if (this.retryBefore !== null && this.retryBefore < floor) this.retryBefore = null;
  }

  /** Read `[from, to]` newest slice first. `newest` marks the read ending at the live edge, which
   * owns `readThrough`. Returns false only when that newest slice failed outright (keep what we had). */
  private async readRange(get: Get, from: number, to: number, newest: boolean, budget: { pages: number }): Promise<boolean> {
    for (let t1 = to; t1 > from; t1 -= SURVEY_SLICE_S) {
      const t0 = Math.max(from, t1 - SURVEY_SLICE_S);
      let cursor: number | null | undefined = 0;
      while (cursor !== null && cursor !== undefined) {
        const overBudget = budget.pages >= this.maxPages;
        const r: ObservationsResp | null = overBudget ? null : await get<ObservationsResp>(
          `/api/observations?f_lo=1000000&f_hi=6000000000&t0=${t0}&t1=${t1}&limit=${this.pageLimit}&cursor=${cursor}`);
        if (!r) {
          // The newest slice failed outright: keep what we had and read it again next refresh.
          if (newest && t1 === to && budget.pages === 0) return false;
          // Otherwise this slice (and everything older) is not fully read: say so, stop here.
          this.truncatedBefore = Math.max(this.truncatedBefore ?? -Infinity, t1);
          if (!overBudget) this.retryBefore = Math.max(this.retryBefore ?? -Infinity, t1);
          if (newest && t1 === to) this.readThrough = null; // not even the newest slice is whole: redo it
          break;
        }
        budget.pages++;
        foldSurveyRecords(this.wins, r.records, 120, r.geometries);
        cursor = r.next_cursor;
      }
      if (cursor !== null && cursor !== undefined) break; // truncated above
      if (newest && t1 === to) this.readThrough = to; // the newest slice is in full; older ones fold in after
    }
    return true;
  }

  private reset() { this.wins = []; this.readThrough = null; this.truncatedBefore = null; this.retryBefore = null; }
}

const fmtAgo = (s: number): string => s < 90 ? `${Math.round(s)} s` : s < 5400 ? `${Math.round(s / 60)} min` : s < 129600 ? `${(s / 3600).toFixed(1)} h` : `${Math.round(s / 86400)} d`;

/** Observed-then windows as rows, newest first. The backend's own record extents; this only
 * groups and words them. A truncated read is stated as its own row (no go-to: nowhere to go). */
export function surveyWindowItems(wins: SurveyWindow[], edgeS: number, max = 4, truncatedBefore: number | null = null): DrawerItem[] {
  const rows: DrawerItem[] = [...wins].sort((a, b) => b.t1 - a.t1).slice(0, max).map((w) => ({
    group: "surveys" as const, tag: w.sweep === undefined ? "survey · observed then" : "survey · swept then",
    title: `${fmtHz(w.lo)} – ${fmtHz(w.hi)}`,
    why: w.sweep === undefined
      ? `the log records this band looked at ${fmtAgo(Math.max(0, edgeS - w.t1))} ago for ${fmtAgo(w.t1 - w.t0)}`
      : `the log records a survey sweep across this range ${fmtAgo(Math.max(0, edgeS - w.t1))} ago over ${fmtAgo(w.t1 - w.t0)}; each step was heard only during its own dwell`,
    hz: (w.lo + w.hi) / 2, spanHz: w.hi - w.lo, time: { t0: w.t0, t1: w.t1 },
  }));
  if (truncatedBefore !== null) {
    rows.push({ group: "surveys", tag: "survey · not fully loaded", title: "Older surveys not loaded",
      why: `the observation log is too dense to read in full here: surveys before ${fmtAgo(Math.max(0, edgeS - truncatedBefore))} ago may be missing`,
      hz: Number.NaN, note: true });
  }
  return rows;
}

/** One-shot form over a single response (kept for callers holding records already). */
export function pastSurveyItems(r: ObservationsResp | null, edgeS: number, max = 4, joinS = 120): DrawerItem[] {
  return r ? surveyWindowItems(foldSurveyRecords([], r.records, joinS, r.geometries), edgeS, max) : [];
}

/** The widest never-looked run of the coverage plane — an honest gap, distinct from observed-then. */
export function neverLookedItems(r: CoverageResp | null): DrawerItem[] {
  const cells = r?.any?.cells;
  if (!r || !cells) return [];
  let best: [number, number] | null = null, start = -1;
  const flush = (end: number) => { if (start >= 0 && (!best || end - start > best[1] - best[0])) best = [start, end]; start = -1; };
  cells.forEach((c, i) => { if (c.state === "unobserved") { if (start < 0) start = i; } else flush(i); });
  flush(cells.length);
  if (!best) return [];
  const [a, b] = best as [number, number];
  const lo = r.grid.f_lo_hz + a * r.grid.f_cell_hz, hi = r.grid.f_lo_hz + b * r.grid.f_cell_hz;
  return [{ group: "surveys", tag: "survey · never looked", title: `${fmtHz(lo)} – ${fmtHz(hi)}`,
    why: "no capture covered this in the window: unobserved, not quiet", hz: (lo + hi) / 2, spanHz: hi - lo }];
}

export function groupItems(items: DrawerItem[]): { group: DrawerGroup; items: DrawerItem[] }[] {
  return GROUP_ORDER.map((g) => ({ group: g, items: items.filter((i) => i.group === g) })).filter((g) => g.items.length > 0);
}

export function peekLine(items: DrawerItem[]): string {
  if (items.length === 0) return "Explore — nothing to suggest yet";
  const n = (g: DrawerGroup) => items.filter((i) => i.group === g).length;
  return `Explore — ${n("unknown")} unknown · ${n("strongest")} strongest · ${n("quiet")} quiet-but-active · ${n("surveys")} past survey`;
}

export const REFRESH_MS = 30_000;

export const mountExploreDrawer: MountFn = (el, ctx) => {
  el.classList.add("explore-drawer");
  el.setAttribute("aria-label", "Explore: places to go");
  const listEl = document.createElement("div");
  const peek = document.createElement("p");
  peek.className = "drawer-peek";
  el.append(peek, listEl);
  let items: DrawerItem[] = [];
  let selected: string | null = null;
  let seq = 0;
  const surveys = new SurveyLog();

  const render = () => {
    listEl.replaceChildren();
    for (const g of groupItems(items)) {
      const h = document.createElement("h4"); h.textContent = GROUP_TITLE[g.group];
      const ul = document.createElement("ul");
      for (const it of g.items) {
        const key = itemKey(it);
        const li = document.createElement("li"), b = document.createElement("button");
        b.type = "button";
        b.className = "row";
        b.setAttribute("aria-pressed", String(key === selected));
        if (key === selected) li.classList.add("selected");
        for (const [cls, text] of [["tag", it.tag], ["f", it.title], ["why", it.why]] as const) {
          const s = document.createElement("span"); s.className = cls; s.textContent = text; b.append(s);
        }
        // P4: the big row only selects (view state); it never moves the map.
        b.addEventListener("click", () => {
          selected = key;
          ctx.store.set(selectItem(it));
          render();
        });
        // The small explicit control is the one that jumps the view.
        const go = document.createElement("button");
        go.type = "button";
        go.className = "go";
        go.textContent = "Go";
        go.setAttribute("aria-label", `Go to ${it.title}`);
        go.setAttribute("title", it.time ? "Pan the map here and review this window" : "Pan the map here");
        go.addEventListener("click", () => {
          selected = key;
          ctx.store.set(gotoItem(it));
          render();
        });
        if (it.note) li.append(b); else li.append(b, go);
        ul.append(li);
      }
      listEl.append(h, ul);
    }
    peek.textContent = peekLine(items);
  };

  const get = async <T,>(path: string): Promise<T | null> => {
    try { return await ctx.client.get<T>(path); } catch { return null; }
  };
  const refresh = async () => {
    const my = ++seq;
    const s = ctx.store.get();
    const edge = s.live.edgeTS;
    const v = s.live.view;
    if (edge === null || !v) return;
    const t0 = edge - 1800;
    const band = `f_lo=${v.loHz}&f_hi=${v.hiHz}`;
    const [ev, st, sch, cov] = await Promise.all([
      get<EventsResp>(`/api/events?${band}&t0=${t0}&t1=${edge}&limit=200`),
      get<StrongestResp>(`/api/analysis/strongest?${band}&window_s=30`),
      get<SchedulerResponse>(`/api/scheduler${schedulerQuery({ fLoHz: v.loHz, fHiHz: v.hiHz, t0, t1: edge })}`),
      get<CoverageResp>(`/api/coverage?f_lo=1000000&f_hi=6000000000&cells=64&t0=${t0}&t1=${edge}`),
      surveys.refresh(get, edge),
    ]);
    if (my !== seq) return;
    items = [...unknownItems(ev), ...strongestItem(st), ...quietItems(sch), ...surveyWindowItems(surveys.wins, edge, 4, surveys.truncatedBefore), ...surveyItems(cov, 2), ...neverLookedItems(cov)];
    render();
  };
  render();
  void refresh();
  setInterval(() => { void refresh(); }, REFRESH_MS);
};
