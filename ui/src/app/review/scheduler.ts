// Scheduler/attention tab (ADR-0013 §2, §8; T-155) over docs/api.md "Attention scheduler". Thin
// client: reuses ui/src/scheduler.ts's wire types, query builder and formatters unchanged; this
// file only renders status, shares, bandit summary, leases, POI/coverage gaps and the (collapsed)
// arm table as the drawer's own DOM. `scheduler: null` (no scheduler on this run) is rendered, not
// treated as an error.
import {
  type Arm, type ArmsResponse, type PoiBand, type SchedulerParams, type SchedulerResponse,
  bandit_summaryText, fmtS, sharesText,
} from "../../scheduler";
import type { ControlClient } from "../../controls/client";
import { h } from "../dom";
import { errText, table, td } from "./util";

export class SchedulerTab {
  private loaded = false;
  private armsLoaded = false;
  private readonly info = h("div", { class: "rv-info hint" });
  private readonly status = h("div", { class: "rv-status" });
  private readonly bandit = h("div", { class: "hint" });
  private readonly leases = h("div", {});
  private readonly poi = h("div", {});
  private readonly armsBody = h("div", { class: "hint" }, "closed");
  private readonly armsDetails = h("details", {}, h("summary", {}, "Bandit arms"), this.armsBody);
  private readonly fLo = h("input", { class: "mono", inputmode: "decimal", placeholder: "any" });
  private readonly fHi = h("input", { class: "mono", inputmode: "decimal", placeholder: "any" });
  private readonly t0 = h("input", { type: "datetime-local", step: "1" });
  private readonly t1 = h("input", { type: "datetime-local", step: "1" });
  private readonly root: HTMLElement;

  constructor(private client: ControlClient) {
    this.armsDetails.addEventListener("toggle", () => { if (this.armsDetails.open && !this.armsLoaded) void this.loadArms(); });
    const form = h("form", { class: "rv-form", onsubmit: (e) => { e.preventDefault(); void this.load(); } },
      h("label", {}, "f lo (MHz)", this.fLo), h("label", {}, "f hi (MHz)", this.fHi),
      h("label", {}, "from (UTC)", this.t0), h("label", {}, "to (UTC)", this.t1),
      h("button", { class: "mini", type: "submit" }, "Load POI"));
    this.root = h("div", { class: "rv-panel" },
      this.status, this.bandit, form, this.info, this.leases, this.poi, this.armsDetails);
  }

  el(): HTMLElement { return this.root; }

  activate() { if (!this.loaded) void this.load(); }

  private params(): SchedulerParams {
    const p: SchedulerParams = {};
    const fLo = +this.fLo.value * 1e6, fHi = +this.fHi.value * 1e6;
    if (Number.isFinite(fLo) && Number.isFinite(fHi) && this.fLo.value && this.fHi.value) { p.fLoHz = fLo; p.fHiHz = fHi; }
    const t0 = Date.parse(`${this.t0.value}Z`) / 1000, t1 = Date.parse(`${this.t1.value}Z`) / 1000;
    if (Number.isFinite(t0) && Number.isFinite(t1) && this.t0.value && this.t1.value) { p.t0 = t0; p.t1 = t1; }
    return p;
  }

  private async load() {
    this.loaded = true;
    this.info.textContent = "loading…";
    try {
      const q = new URLSearchParams();
      const p = this.params();
      if (p.fLoHz !== undefined) { q.set("f_lo", String(p.fLoHz)); q.set("f_hi", String(p.fHiHz)); }
      if (p.t0 !== undefined) { q.set("t0", String(p.t0)); q.set("t1", String(p.t1)); }
      const s = q.toString();
      const r = await this.client.get<SchedulerResponse>(s ? `/api/scheduler?${s}` : "/api/scheduler");
      this.render(r);
      this.info.textContent = "";
    } catch (e) {
      this.info.textContent = errText(e);
    }
  }

  private render(r: SchedulerResponse) {
    const s = r.scheduler;
    if (!s) {
      this.status.textContent = "no scheduler on this run (interactive hk serve, or a plain replay)";
      this.bandit.textContent = "";
    } else {
      this.status.textContent = `now ${fmtS(s.now)} · plan v${s.plan_version} · window ${s.window_s}s · ` +
        `sweep floor ${(s.sweep_floor * 100).toFixed(0)}% ${s.sweep_floor_met ? "met" : `NOT MET (${s.floor_violations} violations)`}` +
        `${s.interactive ? " · interactive" : ""}${s.low_power ? " · low power" : ""} · ${sharesText(s.shares_s)}`;
      this.bandit.textContent = s.bandit ? bandit_summaryText(s.bandit) : "bandit off (plan does not enable extra.bandit)";
    }
    this.leases.replaceChildren(
      h("div", { class: "rv-section-h" }, "Leases", h("em", {}, `${r.leases.length} · observation log ${r.observation_log ? "yes" : "no"}`)),
      table(["id", "kind", "centre", "rate", "duration"], r.leases.map((l) => h("tr", {},
        td(String(l.id)), td(l.kind), td((l.center_hz / 1e6).toFixed(4), "num"), td((l.rate_hz / 1e6).toFixed(3), "num"),
        td(l.duration_s === null ? "held" : `${l.duration_s}s`, "num")))));
    if (!r.span) {
      this.poi.replaceChildren(h("p", { class: "hint" }, "set from/to to compute POI and coverage gaps for a span"));
    } else {
      this.poi.replaceChildren(
        h("div", { class: "rv-section-h" }, "POI", h("em", {}, `${fmtS(r.span.t0)} – ${fmtS(r.span.t1)}${r.poi_truncated ? " (truncated)" : ""}`)),
        table(["region", "observed", "revisit", "POI", "gaps"], r.poi.map((b) => this.poiRow(b))));
    }
  }

  private poiRow(b: PoiBand): HTMLTableRowElement {
    const poiText = b.poi.map((p) => `τ${p.tau_s}s ${(p.p_poi * 100).toFixed(0)}%`).join(", ");
    return h("tr", {},
      td(`${(b.f_lo / 1e6).toFixed(4)}–${(b.f_hi / 1e6).toFixed(4)} MHz`),
      td(`${(b.observed_fraction * 100).toFixed(1)}%`, "num"),
      td(b.mean_revisit_s !== null ? `${b.mean_revisit_s.toFixed(2)}s` : "—", "num"),
      td(poiText), td(`${b.gaps.length}${b.gaps_truncated ? "+" : ""}`, "num"));
  }

  private async loadArms() {
    this.armsLoaded = true;
    this.armsBody.textContent = "loading…";
    try {
      const r = await this.client.get<ArmsResponse>("/api/scheduler/arms");
      if (!r.scheduler) { this.armsBody.textContent = "no scheduler on this run"; return; }
      if (!r.bandit) { this.armsBody.textContent = "bandit off"; return; }
      this.armsBody.replaceChildren(
        h("div", { class: "hint" }, `${r.arms.length} arm${r.arms.length === 1 ? "" : "s"}`),
        table(["#", "centre", "active", "visits", "reward", "ucb", "stale", "suspect"], r.arms.map((a) => this.armRow(a))));
    } catch (e) {
      this.armsBody.textContent = errText(e);
      this.armsLoaded = false;
    }
  }

  private armRow(a: Arm): HTMLTableRowElement {
    return h("tr", {},
      td(String(a.index), "num"), td((a.center_hz / 1e6).toFixed(4), "num"), td(a.active ? "active" : ""),
      td(String(a.visits), "num"), td(a.mean_reward.toFixed(3), "num"),
      td(typeof a.ucb === "number" ? a.ucb.toFixed(3) : a.ucb, "num"),
      td(a.staleness_s.toFixed(1), "num"), td(`${(a.suspect_fraction * 100).toFixed(0)}%`, "num"));
  }
}
