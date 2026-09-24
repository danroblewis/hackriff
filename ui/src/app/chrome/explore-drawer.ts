// T-814 (MAP-14): the Explore drawer — places to go, listed in the bottom sheet (docs/23 §5,
// ui/mockups/map-ui-v1.html `renderExplore`). Four groups: unknown & unexplained (the priority),
// strongest right now, quiet-but-active bands, and past-survey windows.
//
// THIN CLIENT: every figure is the backend's own (GET /api/events, /api/analysis/strongest,
// /api/scheduler POI, /api/coverage); this file only orders, formats and turns a click into a view
// request (`requestGoto` / `reviewAt` — store writes). It issues GETs only and never a device route,
// so opening, scrolling or clicking the drawer cannot move the radio; a frequency outside the tuned
// window is reached by the surface's own retune *offer*, not from here.
import type { MountFn } from "../context";
import { requestGoto } from "../shell-slice";
import { reviewAt } from "../centre/capture-slice";
import { schedulerQuery, type SchedulerResponse } from "../../scheduler";

export type DrawerGroup = "unknown" | "strongest" | "quiet" | "surveys";
export interface DrawerItem {
  group: DrawerGroup; tag: string; title: string; why: string;
  /** Where a click goes: a frequency and, for a past window, the time range to review. */
  hz: number; time?: { t0: number; t1: number };
}
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
      hz: e.f_center_hz,
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
      why: "sampled in the capture window; everything outside is grey (unobserved)", hz: (lo + hi) / 2,
      time: { t0: r.window.t0_s, t1: r.window.t1_s } });
    start = -1;
  };
  cells.forEach((c, i) => { if (c.state === "observed" || c.state === "excluded") { if (start < 0) start = i; } else flush(i); });
  flush(cells.length);
  return out.slice(0, max);
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
  let seq = 0;

  const render = () => {
    listEl.replaceChildren();
    for (const g of groupItems(items)) {
      const h = document.createElement("h4"); h.textContent = GROUP_TITLE[g.group];
      const ul = document.createElement("ul");
      for (const it of g.items) {
        const li = document.createElement("li"), b = document.createElement("button");
        b.type = "button";
        for (const [cls, text] of [["tag", it.tag], ["f", it.title], ["why", it.why]] as const) {
          const s = document.createElement("span"); s.className = cls; s.textContent = text; b.append(s);
        }
        b.addEventListener("click", () => {
          // View arithmetic only: a frequency request the surface pans to (or offers a retune for),
          // and for a past window the review time. Never a device route.
          if (it.time) ctx.store.set(reviewAt(it.time.t1, it.time.t1 - it.time.t0));
          ctx.store.set(requestGoto(it.hz));
        });
        li.append(b); ul.append(li);
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
    ]);
    if (my !== seq) return;
    items = [...unknownItems(ev), ...strongestItem(st), ...quietItems(sch), ...surveyItems(cov)];
    render();
  };
  render();
  void refresh();
  setInterval(() => { void refresh(); }, REFRESH_MS);
};
