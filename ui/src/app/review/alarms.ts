// Alarms tab (ADR-0013 §2, §8; T-155) over docs/api.md "Anomalies and novelty alarms". Thin
// client: reuses ui/src/alarms.ts's wire types, query builder, formatters and dismiss/reopen calls
// unchanged; this file only renders them as the Review drawer's own DOM, in place of the old
// #alarms page section.
import {
  type AnomalyFilters, type AnomalyRow, canDismiss, canReopen, dismissAnomaly, fmtS, freqRangeText,
  loadAnomalies, reopenAnomaly, topExplanationText,
} from "../../alarms";
import type { ControlClient } from "../../controls/client";
import { h } from "../dom";
import { focusSignal, setMode, toast, toggleReview, type AppState } from "../state";
import type { Store } from "../store";
import { errText } from "./util";

/** The anomaly's subject when it is a plain emitter id (own-emitter anomalies carry one; a cell or
 * band subject does not) — an honest read of an already-typed field, never a guessed shape. */
export function alarmEmitterId(a: Pick<AnomalyRow, "subject">): string | null {
  return typeof a.subject === "string" ? a.subject : null;
}

export interface AlarmRowView {
  kind: string; channel: string; site: string; when: string; explanation: string;
  canDismiss: boolean; canReopen: boolean; emitterId: string | null;
}

/** Anomaly row → drawer row view-model: presentation only, every field already served. */
export function alarmRowView(a: AnomalyRow): AlarmRowView {
  const site = a.alarm?.key && (a.alarm.key as { site?: unknown }).site;
  return {
    kind: a.kind,
    channel: freqRangeText(a.f_lo, a.f_hi),
    site: site !== undefined && site !== null && site !== "" ? String(site) : "—",
    when: fmtS(a.t),
    explanation: topExplanationText(a.explanations),
    canDismiss: canDismiss(a),
    canReopen: canReopen(a),
    emitterId: alarmEmitterId(a),
  };
}

const KINDS = ["new-emitter", "busier-than-baseline", "quieter-than-baseline", "noise-floor-rise", "novelty", "level-above-baseline", "change-point"];
const STATUSES = ["open", "resolved", "dismissed"];

export class AlarmsTab {
  private rows: AnomalyRow[] = [];
  private cursor: string | null = null;
  private kind = "";
  private status = "open";
  private loaded = false;
  private readonly info = h("div", { class: "rv-info hint" });
  private readonly list = h("div", { class: "rv-list" });
  private readonly more = h("button", { class: "mini", type: "button", hidden: true, onclick: () => void this.load(true) }, "Load more");
  private readonly root: HTMLElement;

  constructor(private client: ControlClient, private store: Store<AppState>) {
    const kindSel = h("select", { "aria-label": "Kind", onchange: (e) => { this.kind = (e.target as HTMLSelectElement).value; void this.load(); } },
      h("option", { value: "" }, "any kind"), ...KINDS.map((k) => h("option", { value: k }, k)));
    const statusSel = h("select", { "aria-label": "Status", onchange: (e) => { this.status = (e.target as HTMLSelectElement).value; void this.load(); } },
      ...STATUSES.map((s) => h("option", { value: s }, s)), h("option", { value: "" }, "any status"));
    statusSel.value = "open";
    this.root = h("div", { class: "rv-panel" },
      h("div", { class: "rv-filters" },
        kindSel, statusSel,
        h("button", { class: "mini", type: "button", onclick: () => void this.load() }, "Refresh")),
      this.info, this.list, this.more,
    );
  }

  el(): HTMLElement { return this.root; }

  activate() { if (!this.loaded) void this.load(); }

  private async load(more = false) {
    if (!more) { this.rows = []; this.cursor = null; }
    this.loaded = true;
    this.info.textContent = "loading…";
    try {
      const filters: AnomalyFilters = { kind: this.kind || undefined, status: this.status || undefined };
      const page = await loadAnomalies(this.client, filters, more ? this.cursor : null);
      this.rows.push(...page.anomalies);
      this.cursor = page.next_cursor;
      this.more.hidden = !this.cursor;
      this.info.textContent = `${this.rows.length} loaded${this.cursor ? " (more on the server)" : ""}`;
      this.render();
    } catch (e) {
      this.info.textContent = errText(e);
    }
  }

  private render() {
    if (!this.rows.length) { this.list.replaceChildren(h("div", { class: "empty" }, "No alarms match this filter.")); return; }
    this.list.replaceChildren(...this.rows.map((a) => this.row(a)));
  }

  private row(a: AnomalyRow): HTMLElement {
    const v = alarmRowView(a);
    const acts = h("div", { class: "rv-acts" });
    if (v.emitterId) acts.append(h("button", { class: "mini", type: "button", onclick: () => this.focus(v.emitterId!) }, "Focus in Explore"));
    if (v.canDismiss) acts.append(h("button", { class: "mini", type: "button", onclick: () => void this.dismiss(a.id) }, "Dismiss"));
    if (v.canReopen) acts.append(h("button", { class: "mini", type: "button", onclick: () => void this.reopen(a.id) }, "Reopen"));
    return h("div", { class: "rv-row", "data-status": a.status },
      h("div", { class: "rv-row-head" },
        h("span", { class: "chip" }, v.kind),
        h("span", { class: "mono" }, v.channel),
        h("span", { class: "hint" }, v.site === "—" ? "—" : `site ${v.site}`),
        h("span", { class: "hint" }, v.when)),
      h("div", { class: "rv-row-expl" }, v.explanation),
      acts);
  }

  /** "It links to Explore focus via a store action" (T-155 brief): focuses the emitter, switches
   * to Explore, and closes the drawer — three already-defined store actions, nothing computed here. */
  private focus(emitterId: string) {
    this.store.set(focusSignal(emitterId));
    this.store.set(setMode("explore"));
    this.store.set(toggleReview);
  }

  private async dismiss(id: string) {
    try {
      await dismissAnomaly(this.client, id);
      this.store.set(toast("Alarm dismissed."));
      await this.load();
    } catch (e) {
      this.store.set(toast(`dismiss: ${errText(e)}`));
    }
  }

  private async reopen(id: string) {
    try {
      await reopenAnomaly(this.client, id);
      this.store.set(toast("Alarm reopened."));
      await this.load();
    } catch (e) {
      this.store.set(toast(`reopen: ${errText(e)}`));
    }
  }
}
