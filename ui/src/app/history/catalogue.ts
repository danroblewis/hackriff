// History surface: wire types, the query builder and the formatting the catalogue renders
// (T-264, ADR-0017 stage TM-8). Owner: T-264. No DOM, no fetch — unit-tested.
//
// THIS FILE DECIDES NOTHING ABOUT SIGNALS. Which events exist, how long each lasted, whether the
// period was observed at all and what an empty catalogue means are all the backend's answers
// (docs/api.md `GET /api/events`); what is here is region/range choice and presentation, per the
// thin-client rule. In particular `duration_s` is served, never derived from `t_start_s`/`t_end_s`
// here, and the empty-state wording is the backend's own `coverage.statement`.
import { currentSpan } from "../centre/capture-window";

/** One event: a presence interval, with its own timespan (docs/api.md `GET /api/events`). */
export interface CatalogueEvent {
  emitter_id: string;
  t_start_s: number; t_end_s: number;
  /** The event's own length, in seconds, as the backend computed it. */
  duration_s: number;
  /** Its intersection with the requested window. */
  in_window_s: number;
  open: boolean; count: number; sources: number;
  f_center_hz: number | null;
}

/** One ranked explanation, as `/api/events` serves it beside an emitter. */
export interface CatalogueExplanation { rank: number; service: string; label: string; score: number; flags: string[] }

/** The emitter an event belongs to, listed once per answer. */
export interface CatalogueEmitter {
  id: string; state: string;
  f_center_hz: number; bandwidth_hz: number; f_lo_hz: number; f_hi_hz: number;
  known_status: string; family: string | null;
  explanations: CatalogueExplanation[];
  identity_scheme: string | null; identity_value?: string; withheld: boolean;
  /** Events and time on air for this emitter over the whole window (never this page). */
  events: number; on_air_s: number;
  /** The lifetime History total — never a liveness or ranking input (ADR-0017 §5). */
  count: number;
}

/** What the receiver actually observed over the queried box. `statement` is backend-rendered and
 * is the sentence shown beside an empty catalogue: it keeps "coverage unknown", "no data for this
 * period" and "nothing was on the air" apart, and never lets the first two read as the third. */
export interface Coverage {
  source: string | null;
  observed_fraction: number | null;
  cells: number | null; observed_cells: number | null;
  gaps: { t0_s: number; t1_s: number }[] | null;
  gaps_truncated: boolean | null;
  statement: string;
}

export interface CataloguePage {
  window: { f_lo_hz: number; f_hi_hz: number; t0_s: number; t1_s: number };
  events: CatalogueEvent[];
  emitters: CatalogueEmitter[];
  total: number; limit: number; next_cursor: string | null;
  emitters_truncated: boolean; emitters_no_interval: number;
  coverage: Coverage;
}

/** One interval of `GET /api/inventory/{id}/presence`. */
export interface TrackInterval {
  t_start_s: number; t_end_s: number; duration_s: number;
  open: boolean; count: number; sources: number; f_center_hz: number | null;
}
export interface PresenceTrack {
  emitter: string;
  window: { t0_s: number; t1_s: number } | null;
  intervals: TrackInterval[];
  total: number; truncated: boolean;
}

export interface Region { fLoHz: number; fHiHz: number; t0: number; t1: number }

/** The period the surface opens on. History is the all-time record, so this is only where the
 * *first* look starts; the form takes it anywhere. */
export const DEFAULT_SPAN_S = 24 * 3600;

/**
 * **This surface is deliberately NOT scoped to the view window** (T-386), and says so on its face.
 *
 * T-386 asked the prior question of the whole-UI window rule for the four surfaces T-379/T-384 left
 * on a clock of their own, and this one answers *independent*, for four reasons:
 *
 * 1. CLAUDE.md's own invariant puts it outside: Explore is "time-scoped to the view", while "the
 *    durable all-time record lives in a **separate** history surface (workflow #3), where ephemera
 *    are catalogued as past events with a timespan".
 * 2. Workflow #3 is "**choose a region** and see what activity was seen there over time". Choosing
 *    the period *is* the surface's function; the form is its window control. Slaving it to the
 *    cursor would delete the feature, not scope it.
 * 3. The view window is bounded by the IQ ring's retention — seconds to minutes. The catalogue's
 *    whole point is the periods beyond that, so following the cursor would make most of the durable
 *    record unreachable from the surface that exists to reach it.
 * 4. It is a separate **mode**, not a panel beside a scrubbed waterfall, so the adjacency that made
 *    the live-only panels misleading (T-387) does not apply.
 *
 * What *is* borrowed from T-387 is the honesty move: a surface that answers about its own window
 * must **say which window**, standingly, not only when it happens to be empty.
 */
export const INDEPENDENT_PERIOD_NOTE =
  "History is the durable all-time catalogue: it answers about the period in this form, not the window the waterfall is showing.";

/** Why the surface could not open on a period of its own — two different missing answers, and the
 * note must not render one as the other. */
export type DefaultRegion =
  | { kind: "region"; region: Region }
  /** Nothing is tuned and no view exists, so there is no band to open on. */
  | { kind: "no-band" }
  /** No capture clock has been reported, so there is no honest instant to end the period at. */
  | { kind: "no-clock" };

/**
 * The region and period to open on: the live view's own span (else the device's tuned band) over
 * the [[DEFAULT_SPAN_S]] ending at `edgeS`. Already-known UI state only — no measurement here.
 *
 * `edgeS` is the **capture clock's** live edge (`explore/inventory.ts` `liveEdgeS`), never
 * `Date.now()`. This surface opened on `Date.now() / 1000` until T-386, which is the sixth site of
 * the bug T-379 found: on a fixture 3.5 days from wall time the catalogue opened on a 24 h period
 * the capture never covered, so it loaded an empty answer about a window nothing had ever sampled
 * and put the user in front of it as the surface's first impression. A null edge is *unknown* and
 * the form is left for the user to fill, because inventing the period is what caused it.
 */
export function defaultRegion(
  input: { live: { loHz: number; hiHz: number } | null; device: { centerHz: number | null; sampleRateHz: number | null } },
  edgeS: number | null,
): DefaultRegion {
  const span = currentSpan(input);
  if (span === null) return { kind: "no-band" };
  if (edgeS === null || !Number.isFinite(edgeS)) return { kind: "no-clock" };
  return { kind: "region", region: { fLoHz: span.loHz, fHiHz: span.hiHz, t0: edgeS - DEFAULT_SPAN_S, t1: edgeS } };
}

/** What the surface says when it cannot open on a period of its own. */
export function defaultRegionNote(d: Exclude<DefaultRegion, { kind: "region" }>): string {
  return d.kind === "no-band"
    ? "Tune the receiver, or type a region and period."
    : "No capture clock reported yet — type a period to search the catalogue.";
}

/** `GET /api/events` for a region, a lifecycle filter and an optional page cursor. */
export function eventsQuery(r: Region, state: string, cursor: string | null = null, limit = 200): string {
  const p = new URLSearchParams();
  p.set("f_lo", String(r.fLoHz));
  p.set("f_hi", String(r.fHiHz));
  p.set("t0", String(r.t0));
  p.set("t1", String(r.t1));
  if (state) p.set("state", state);
  p.set("limit", String(limit));
  if (cursor) p.set("cursor", cursor);
  return `/api/events?${p}`;
}

/** `GET /api/inventory/{id}/presence` over the same period. */
export function trackQuery(id: string, r: Region): string {
  return `/api/inventory/${encodeURIComponent(id)}/presence?t0=${r.t0}&t1=${r.t1}`;
}

/** A timespan in words. A one-off burst reads in milliseconds rather than rounding to "0 s" — the
 * measured length, whatever it is, because an ephemeral emission is a first-class event. */
export function lastedText(durationS: number): string {
  if (!Number.isFinite(durationS) || durationS < 0) return "—";
  if (durationS < 1) return `${Math.round(durationS * 1000)} ms`;
  if (durationS < 60) return `${durationS.toFixed(1)} s`;
  if (durationS < 3600) return `${(durationS / 60).toFixed(1)} min`;
  return `${(durationS / 3600).toFixed(1)} h`;
}

export interface EventRowView {
  id: string; when: string; lasted: string; freq: string;
  /** "still on air at the end of this period" — the backend's `open`, never re-derived here. */
  open: boolean;
  what: string; state: string; sightings: string;
}

/** An event plus its emitter, as the catalogue row shows them. Presentation only. */
export function eventRowView(ev: CatalogueEvent, emitter: CatalogueEmitter | undefined): EventRowView {
  const hz = ev.f_center_hz ?? emitter?.f_center_hz ?? null;
  const top = emitter?.explanations?.[0];
  return {
    id: ev.emitter_id,
    when: new Date(ev.t_start_s * 1000).toISOString().replace("T", " ").slice(0, 19),
    lasted: lastedText(ev.duration_s),
    freq: hz === null ? "—" : `${(hz / 1e6).toFixed(4)} MHz`,
    open: ev.open,
    // A suggestion, in the order the backend ranked it — never presented as what the signal is.
    what: emitter?.identity_value ?? emitter?.family ?? (top ? `${top.label}?` : "unknown"),
    state: emitter?.state ?? "",
    sightings: `${ev.count}`,
  };
}

/**
 * What to show when the catalogue has no rows. The distinction this protects is the whole point of
 * the coverage block: *no data for this period* is the absence of a measurement, *nothing was on
 * the air* is one. Neither is ever rendered as the other, and the words come from the backend.
 */
export function emptyText(page: CataloguePage | null, error: string | null): string {
  if (error) return error;
  if (!page) return "Choose a region and a period.";
  if (page.total > 0) return "";
  return page.coverage.statement;
}

/** The line above the list: what this answer covers, and what it had to leave out. */
export function summaryText(page: CataloguePage): string {
  const parts = [`${page.total} event${page.total === 1 ? "" : "s"}`, `${page.emitters.length} emitter${page.emitters.length === 1 ? "" : "s"}`];
  if (page.events.length < page.total) parts.push(`showing ${page.events.length}`);
  if (page.emitters_truncated) parts.push("more emitters matched than were expanded");
  if (page.emitters_no_interval > 0) parts.push(`${page.emitters_no_interval} row(s) here carry no presence interval, so they contribute no event`);
  return parts.join(" · ");
}
