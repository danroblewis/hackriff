// Survey report tab (ADR-0013 §2, §8; T-155) over docs/api.md "Survey reports". Thin client:
// reuses ui/src/report.ts's wire types, query builder and formatters unchanged; this file only
// renders a coverage/POI disclosure, source/site filters and warnings as the drawer's own DOM
// (`source` is a documented `/api/report` filter the frozen ReportParams type omits, so the query
// string is extended here rather than editing that file).
import {
  type ChangeVsBaseline, type OccupancyRow, type ReportCoverage, type ReportParams, type SurveyReport,
  type TopEmitter, changeStatusText, fcoText, fmtNs, freqText, reportExportUrl, reportQuery,
} from "../../report";
import type { ControlClient } from "../../controls/client";
import type { TimeCursor } from "../centre/capture-slice";
import type { LiveSlice } from "../centre/slice";
import { h } from "../dom";
import type { AppState, ReviewSlice } from "../state";
import type { Store } from "../store";
import { errText, table, td } from "./util";

/** The form's default region/span: the region the drawer was opened on (e.g. a selection), else
 * the current live view; the span defaults to the last hour up to now (or the reviewed instant
 * while reviewing). Already-known UI state only, never a measurement. */
export function defaultReportParams(
  region: ReviewSlice["region"], live: Pick<LiveSlice, "view">, time: TimeCursor, nowS: number,
): ReportParams | null {
  const box = region ?? (live.view ? { loHz: live.view.loHz, hiHz: live.view.hiHz } : null);
  if (!box) return null;
  const t1 = region?.t1 ?? (time.live ? nowS : time.tS);
  const t0 = region?.t0 ?? t1 - 3600;
  return { fLoHz: box.loHz, fHiHz: box.hiHz, t0, t1 };
}

function reportUrl(p: ReportParams, source: string, format?: "csv" | "png"): string {
  const base = format ? reportExportUrl(p, format) : reportQuery(p);
  return source ? `${base}&source=${encodeURIComponent(source)}` : base;
}

export class ReportTab {
  private lastRegionKey = "";
  private readonly info = h("div", { class: "rv-info hint" });
  private readonly body = h("div", { class: "rv-report-body", hidden: true });
  private readonly warnBox = h("ul", { class: "rv-warnings" });
  private readonly fLo = h("input", { class: "mono", inputmode: "decimal", placeholder: "100.000" });
  private readonly fHi = h("input", { class: "mono", inputmode: "decimal", placeholder: "102.000" });
  private readonly t0 = h("input", { type: "datetime-local", step: "1" });
  private readonly t1 = h("input", { type: "datetime-local", step: "1" });
  private readonly site = h("input", { class: "mono", placeholder: "unassigned" });
  private readonly source = h("input", { class: "mono", placeholder: "16 hex digits, or unknown" });
  private readonly exports = h("div", { class: "rv-exports", hidden: true });
  private readonly root: HTMLElement;

  constructor(private client: ControlClient, private store: Store<AppState>, private token: string) {
    const form = h("form", { class: "rv-form", onsubmit: (e) => { e.preventDefault(); void this.load(); } },
      h("label", {}, "f lo (MHz)", this.fLo), h("label", {}, "f hi (MHz)", this.fHi),
      h("label", {}, "from (UTC)", this.t0), h("label", {}, "to (UTC)", this.t1),
      h("label", {}, "site", this.site), h("label", {}, "source", this.source),
      h("button", { class: "mini", type: "submit" }, "Load report"));
    this.root = h("div", { class: "rv-panel" }, form, this.info, this.exports, this.body);
  }

  el(): HTMLElement { return this.root; }

  activate(region: ReviewSlice["region"]) {
    const key = region ? JSON.stringify(region) : "";
    if (key === this.lastRegionKey && this.info.textContent) return;
    this.lastRegionKey = key;
    const live = this.store.get().live, time = this.store.get().time;
    const p = defaultReportParams(region, live, time, Date.now() / 1000);
    if (!p) { this.info.textContent = "open a region in Explore, or set f lo/f hi/from/to below."; return; }
    this.fLo.value = (p.fLoHz / 1e6).toFixed(6);
    this.fHi.value = (p.fHiHz / 1e6).toFixed(6);
    this.t0.value = new Date(p.t0 * 1000).toISOString().slice(0, 19);
    this.t1.value = new Date(p.t1 * 1000).toISOString().slice(0, 19);
    void this.load();
  }

  private params(): ReportParams | null {
    const fLo = +this.fLo.value * 1e6, fHi = +this.fHi.value * 1e6;
    const t0 = Date.parse(`${this.t0.value}Z`) / 1000, t1 = Date.parse(`${this.t1.value}Z`) / 1000;
    if (![fLo, fHi, t0, t1].every(Number.isFinite) || fHi <= fLo || t1 <= t0) return null;
    return { fLoHz: fLo, fHiHz: fHi, t0, t1, site: this.site.value.trim() || undefined };
  }

  private async load() {
    const p = this.params();
    if (!p) { this.info.textContent = "set f lo, f hi, from and to"; return; }
    this.info.textContent = "loading…";
    this.body.hidden = true;
    const src = this.source.value.trim();
    try {
      const r = await this.client.get<SurveyReport>(reportUrl(p, src));
      this.info.textContent = `generated ${fmtNs(r.generated_at_ns)} · ${freqText(r.region)}`;
      this.exports.hidden = false;
      this.exports.replaceChildren(
        h("a", { class: "mini", href: `${reportUrl(p, src, "csv")}&token=${encodeURIComponent(this.token)}` }, "CSV"),
        h("a", { class: "mini", href: `${reportUrl(p, src, "png")}&token=${encodeURIComponent(this.token)}` }, "PNG"));
      this.body.hidden = false;
      this.body.replaceChildren(
        this.coverage(r.coverage),
        this.occupancy(r.occupancy),
        this.emitters(r.top_emitters),
        this.changes(r.change_vs_baseline));
      this.warnBox.replaceChildren(...r.warnings.map((w) => h("li", {}, w)));
      this.body.append(h("div", { class: "rv-section-h" }, "Warnings"), this.warnBox);
    } catch (e) {
      this.info.textContent = errText(e);
    }
  }

  /** Coverage and POI are shown first (CLAUDE.md/docs/api.md: unobserved is never reported as quiet). */
  private coverage(c: ReportCoverage): HTMLElement {
    return h("div", { class: "rv-coverage" },
      h("div", { class: "rv-section-h" }, "Coverage"),
      h("p", {}, c.statement),
      h("p", { class: "hint" },
        `observed ${(c.observed_fraction * 100).toFixed(1)}% · ${c.observed_s.toFixed(1)} s` +
        (c.gaps.length ? ` · ${c.gaps.length} gap${c.gaps.length === 1 ? "" : "s"}${c.gaps_truncated ? " (truncated)" : ""}` : "") +
        (c.never_observed.length ? ` · ${c.never_observed.length} never observed` : "")),
      table(["τ", "P(POI)"], c.poi.map((p) => h("tr", {}, td(`${p.tau_s} s`, "num"), td(`${(p.p_poi * 100).toFixed(1)}%`, "num")))));
  }

  private occupancy(o: SurveyReport["occupancy"]): HTMLElement {
    const rows = (rs: OccupancyRow[]) => rs.map((r) => h("tr", {},
      td(freqText(r.subject_extent)), td(fcoText(r), "num"),
      td(r.n_revisits !== undefined ? String(r.n_revisits) : "—", "num"), td(r.revisit_biased ? "biased" : "")));
    return h("div", {},
      h("div", { class: "rv-section-h" }, "Occupancy", o.truncated ? h("em", {}, "truncated to 64 rows") : ""),
      table(["extent", "fco", "revisits", ""], rows(o.bands).concat(rows(o.channels))));
  }

  private emitters(list: TopEmitter[]): HTMLElement {
    return h("div", {},
      h("div", { class: "rv-section-h" }, "Top emitters", h("em", {}, `${list.length}`)),
      table(["emitter", "extent", "lifecycle", "sightings", "suggestion"], list.map((e) => h("tr", {},
        td(e.emitter_id), td(freqText(e.freq)), td(e.lifecycle),
        td(String(e.sightings), "num"), td(e.top_suggestion ? `${e.top_suggestion} (suggestion)` : "—")))));
  }

  private changes(cvb: ChangeVsBaseline): HTMLElement {
    return h("div", {},
      h("div", { class: "rv-section-h" }, "Change vs baseline", h("em", {}, changeStatusText(cvb.status))),
      cvb.changes.length
        ? table(["kind", "baseline", "observed", "z"], cvb.changes.map((c) => h("tr", {},
            td(c.kind), td(c.baseline.toFixed(2), "num"), td(c.observed.toFixed(2), "num"), td(c.z.toFixed(2), "num"))))
        : h("p", { class: "hint" }, "no comparison available"));
  }
}
