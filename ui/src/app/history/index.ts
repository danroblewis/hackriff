// History surface mount (T-264, ADR-0017 stage TM-8; CLAUDE.md product vision workflow #3):
// the durable all-time record, where Explore is scoped to the viewed window.
//
// One region, one period, every event that happened there — one row per presence interval, so a
// one-off burst is a row with its own timespan rather than something that had to be a live
// candidate to be recorded. Nothing here filters by what a signal "is": the backend's `/api/events`
// answer is the catalogue, and this file chooses the box and draws the answer.
//
// T-386 — **this surface does not follow the view window, on purpose**, and the reasoning is in
// `catalogue.ts` beside `INDEPENDENT_PERIOD_NOTE`. Two things follow from that decision and are
// implemented here: the surface *states* which window it answers about, standingly; and the period
// it opens on ends at the **capture clock's** live edge rather than `Date.now()`.
import { fmtT, fromUtcInput, utcInput } from "../../history";
import type { AppContext, AreaMounts, MountFn } from "../context";
import { h } from "../dom";
import { apiErrorText } from "../explore/format";
import { liveEdgeS } from "../explore/inventory";
import { focusSignal, setMode } from "../state";
import {
  INDEPENDENT_PERIOD_NOTE, defaultRegion, defaultRegionNote, emptyText, eventRowView, eventsQuery,
  lastedText, summaryText, trackQuery,
  type CataloguePage, type CatalogueEmitter, type PresenceTrack, type Region,
} from "./catalogue";

const STATES = [
  { value: "", label: "candidates + confirmed" },
  { value: "confirmed", label: "confirmed only" },
  { value: "candidate", label: "candidates only" },
  { value: "deleted", label: "deleted rows" },
];

class CatalogueSurface {
  private page: CataloguePage | null = null;
  private error: string | null = null;
  private cursor: string | null = null;
  private loaded = false;
  private readonly fLo = h("input", { class: "mono", inputmode: "decimal", "aria-label": "From (MHz)" });
  private readonly fHi = h("input", { class: "mono", inputmode: "decimal", "aria-label": "To (MHz)" });
  private readonly t0 = h("input", { type: "datetime-local", step: "1", "aria-label": "From (UTC)" });
  private readonly t1 = h("input", { type: "datetime-local", step: "1", "aria-label": "To (UTC)" });
  private readonly state = h("select", { "aria-label": "Rows" }, ...STATES.map((s) => h("option", { value: s.value }, s.label)));
  private readonly note = h("div", { class: "hist-note hint" });
  private readonly coverage = h("div", { class: "hist-note hint" });
  /** T-386: the standing statement of which window this surface answers about. Always shown —
   * never only when the list is empty — because the claim is about the surface, not the answer. */
  private readonly scope = h("div", { class: "hist-note hint" }, INDEPENDENT_PERIOD_NOTE);
  private readonly list = h("div", { class: "hist-list" });
  private readonly more = h("button", { class: "mini", type: "button", hidden: true, onclick: () => void this.load(true) }, "Load more");
  private readonly root: HTMLElement;

  constructor(private ctx: AppContext) {
    this.root = h("div", {},
      h("div", { class: "section-h" }, "History", h("em", {}, "every event recorded in a region, over time")),
      h("form", {
        class: "hist-form",
        onsubmit: (e: Event) => { e.preventDefault(); void this.load(); },
      },
        h("label", {}, "From (MHz)", this.fLo),
        h("label", {}, "To (MHz)", this.fHi),
        h("label", {}, "From (UTC)", this.t0),
        h("label", {}, "To (UTC)", this.t1),
        h("label", {}, "Rows", this.state),
        h("button", { class: "mini", type: "submit" }, "Show")),
      this.scope, this.note, this.coverage, this.list, this.more);
  }

  el(): HTMLElement { return this.root; }

  /**
   * First switch to the surface fills the form from what is already on screen and loads.
   *
   * The period ends at the **capture clock's** live edge, not the browser's (T-386): this surface
   * used to open on `Date.now()`, so on a capture running away from wall time it opened on a day
   * the receiver was never switched on for. With no capture clock reported it opens on no period at
   * all and says so — an unfilled form is honest, an invented day is not.
   *
   * `loaded` latches only on success, so waiting for the clock is not a one-shot: the caller calls
   * this again when a band or a live edge is first reported, and a surface the user opened early
   * fills itself in rather than sitting on its own excuse until the page is reloaded.
   */
  activate() {
    if (this.loaded) return;
    const s = this.ctx.store.get();
    const d = defaultRegion({ live: s.live.view, device: s.device }, liveEdgeS(s));
    if (d.kind !== "region") { this.note.textContent = defaultRegionNote(d); return; }
    this.loaded = true;
    this.fill(d.region);
    void this.load();
  }

  private fill(r: Region) {
    this.fLo.value = (r.fLoHz / 1e6).toFixed(4);
    this.fHi.value = (r.fHiHz / 1e6).toFixed(4);
    this.t0.value = utcInput(r.t0);
    this.t1.value = utcInput(r.t1);
  }

  /** The form's region, or null when it is not a usable box (the server would refuse it). */
  private region(): Region | null {
    const fLoHz = Number(this.fLo.value) * 1e6, fHiHz = Number(this.fHi.value) * 1e6;
    const t0 = fromUtcInput(this.t0.value), t1 = fromUtcInput(this.t1.value);
    if (![fLoHz, fHiHz, t0, t1].every(Number.isFinite) || fHiHz <= fLoHz || t1 <= t0) return null;
    return { fLoHz, fHiHz, t0, t1 };
  }

  private async load(more = false) {
    const r = this.region();
    if (!r) { this.note.textContent = "Needs a frequency range and a period (to after from)."; return; }
    if (!more) { this.page = null; this.cursor = null; }
    this.error = null;
    this.note.textContent = "loading…";
    try {
      const page = await this.ctx.client.get<CataloguePage>(eventsQuery(r, this.state.value, more ? this.cursor : null));
      this.page = this.page && more ? { ...page, events: [...this.page.events, ...page.events] } : page;
      this.cursor = page.next_cursor;
      this.more.hidden = !this.cursor;
    } catch (e) {
      this.error = apiErrorText(e);
    }
    this.render(r);
  }

  private render(r: Region) {
    const page = this.page;
    this.note.textContent = this.error ?? (page ? summaryText(page) : "");
    // Coverage is shown whether or not the list is empty: a sparsely observed period explains a
    // short catalogue just as much as an empty one.
    this.coverage.textContent = page && !this.error ? page.coverage.statement : "";
    const empty = emptyText(page, this.error);
    if (!page || page.events.length === 0) {
      this.list.replaceChildren(h("div", { class: "empty" }, empty));
      return;
    }
    const byId = new Map<string, CatalogueEmitter>(page.emitters.map((m) => [m.id, m]));
    this.list.replaceChildren(...page.events.map((ev) => this.row(ev, byId.get(ev.emitter_id), r)));
  }

  private row(ev: CataloguePage["events"][number], emitter: CatalogueEmitter | undefined, r: Region): HTMLElement {
    const v = eventRowView(ev, emitter);
    const track = h("div", { class: "hist-track hint" });
    const el = h("div", { class: "hist-ev", "data-open": String(v.open) },
      h("span", { class: "mono" }, v.when),
      h("span", { class: "mono" }, v.freq),
      h("span", {}, `lasted ${v.lasted}`),
      v.open ? h("span", { class: "chip" }, "still on air") : null,
      h("span", { class: "hint" }, v.what),
      v.state ? h("span", { class: "chip" }, v.state) : null,
      h("span", { class: "hint" }, `${v.sightings} sightings`),
      h("div", { class: "hist-acts" },
        h("button", { class: "mini", type: "button", onclick: () => void this.showTrack(v.id, r, track) }, "Track"),
        h("button", { class: "mini", type: "button", onclick: () => this.focus(v.id) }, "Focus in Explore")),
      track);
    return el;
  }

  /** The emitter's whole presence track over the same period: every interval, each with its own
   * timespan (docs/api.md `GET /api/inventory/{id}/presence`). */
  private async showTrack(id: string, r: Region, into: HTMLElement) {
    into.textContent = "loading…";
    try {
      const t = await this.ctx.client.get<PresenceTrack>(trackQuery(id, r));
      const shown = t.intervals.slice(0, 12).map((i) => `${fmtT(i.t_start_s)} · ${lastedText(i.duration_s)}${i.open ? " · open" : ""}`);
      into.textContent = t.total === 0
        ? "no intervals in this period"
        : `${t.total} interval${t.total === 1 ? "" : "s"}: ${shown.join(" | ")}${t.total > shown.length ? " …" : ""}`;
    } catch (e) {
      into.textContent = apiErrorText(e);
    }
  }

  private focus(id: string) {
    this.ctx.store.set(focusSignal(id));
    this.ctx.store.set(setMode("explore"));
  }
}

const mountCatalogue: MountFn = (el, ctx) => {
  const surface = new CatalogueSurface(ctx);
  el.replaceChildren(surface.el());
  const activateHere = () => { if (ctx.store.get().mode === "history") surface.activate(); };
  ctx.store.select((s) => s.mode, activateHere, { immediate: true });
  // T-386: and again when a capture clock or a band is first reported. Opening this surface before
  // the first `/api/timeline` answer used to leave it permanently on "no period"; it now fills in
  // the moment there is an honest period to fill it with.
  ctx.store.select((s) => `${liveEdgeS(s)}|${s.live.view?.loHz}|${s.device.centerHz}`, activateHere);
};

export const mounts: AreaMounts = { catalogue: mountCatalogue };
