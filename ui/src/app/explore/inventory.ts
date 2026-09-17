// Inventory row model, queries and actions (ADR-0013 §4.2, §4.5, T-151). Reuses the old page's
// pure helpers (query building, promote/delete, T-080's per-state split) where their shape still
// fits, and adds the richer row fields docs/api.md `/api/inventory` serves — `classification`,
// `explanations`, `refined` — that the old page's `Row` never needed.
import { deleteEntry, inventoryQuery, promoteEntry, rowListenTarget, type ActionResult, type Filters, type InventoryClient, type Recurrence as BaseRecurrence, type Row as BaseRow, type UserBand } from "../../inventory";
import { surveyCells, type CoverageResponse } from "../../navigators";
import { WATERFALL_ROWS } from "../../waterfall";
import type { AppContext } from "../context";
import { apiErrorText } from "./format";
import { setInventoryRows, setInventoryWindow } from "./slice";
import type { InventoryTab, InventorySortKey, WindowCoverage } from "./slice";

export { promoteEntry, deleteEntry, rowListenTarget };
export type { ActionResult, Filters, UserBand };

/** One `recurrence.recent[]` window (docs/api.md `/api/inventory`). */
export interface RecurrenceAppearance { t_start_s: number; t_end_s: number; count: number; duty_cycle: number }

/** `hk-model` recurrence stats plus `recent[]` (the old page's [[BaseRecurrence]] predates that
 * field; the candidate rows' activity dots need it). */
export interface Recurrence extends BaseRecurrence { recent: RecurrenceAppearance[] }

/** `family::ExplanationEvidence`, as `/api/inventory` serves it (`kind` is the Rust enum's
 * `#[serde(tag = "kind", rename_all = "kebab-case")]`). */
export type ExplanationEvidence =
  | { kind: "family"; family: string; model_version: string; confidence: number; mapping_confidence: number }
  | { kind: "band-plan"; status: string; prior_ref: string | null; reason: string }
  | {
      kind: "raster"; raster_hz: number; nearest_channel_hz: number; offset_hz: number;
      tolerance_hz: number; on_raster: boolean; source: string; center_source: string;
    };

/** One ranked explanation (`family::Explanation`). */
export interface Explanation {
  rank: number; service: string; label: string; score: number;
  evidence_confidence: number; status_evidence_confidence: number;
  status: string; prior_ref: string | null; flags: string[]; evidence: ExplanationEvidence[];
}

/** One posterior label of a `Classification.top` distribution, or a within-family `class`. */
export interface PosteriorLabel { label: string; p: number }

/** `hk_model::RecordedClassification`, as `/api/inventory` serves it (T-211/T-199, ADR-0016 §2).
 * `stage`/`arb_rank` are always set (derived for a pre-M3 row); `taxonomy`, `coarse`, `class`,
 * `top`, `entropy_norm` and `flags` are `null` on a pre-M3 row or when nothing has scored this
 * emitter's distribution yet. `open_set_score` is the mass the classifier put on "not measured
 * well enough to name" — present whenever `family` is, even when `family` itself is not
 * `"unknown"`. */
export interface Classification {
  family: string; confidence: number; open_set_score: number; model_version: string; t_s: number;
  taxonomy: string | null;
  stage: "feature-tree" | "verifier" | "dl" | "decoder" | "user" | "chain" | "track-shape" | null;
  arb_rank: number | null;
  coarse: "analog" | "digital" | "noise-like" | "unknown" | null;
  /** The winning label within `family`, below its own confidence gate when `null`. */
  class: (PosteriorLabel & { stage: string }) | null;
  /** Up to 5 posterior labels, highest first, `unknown` included — the classification
   * distribution the focus panel shows (T-207). */
  top: PosteriorLabel[] | null;
  entropy_norm: number | null;
  flags: string[] | null;
}

/** Output-refined tuning (hk-model `RefinedTuning`, T-070), the emitter's `refined` field. */
export interface RefinedTuning {
  emitter_id: string; provenance: string; objective: string; mode: string; source: string;
  center_hz: number; bandwidth_hz: number; start_center_hz: number; start_bandwidth_hz: number;
  detected_center_hz: number; detected_bandwidth_hz: number; objective_value: number;
  locked: boolean; converged: boolean; t_s: number;
}

/** An `/api/inventory` row: the old page's row plus the focus-panel-only fields, and the richer
 * [[Recurrence]] (with `recent[]`) the sidebar's activity dots need. */
export interface Row extends Omit<BaseRow, "recurrence"> {
  recurrence: Recurrence | null;
  classification: Classification | null;
  explanations: Explanation[];
  refined: RefinedTuning | null;
  /** The C18 cluster of unknown emissions this row currently belongs to ("I have seen this
   * before"), or `null` without one — no cluster yet, not visible (< 3 members/appearances), or
   * a withheld-identity row (docs/api.md `cluster_id`, T-202, ADR-0016 §5). It is a *type*
   * ("the same thing I saw before"), never an identity: sets nothing else on the row. */
  cluster_id: string | null;
}

export interface Page { entries: Row[]; next_cursor: string | null }

/** Loads one tab's page, reusing the old page's query builder (T-080: candidates and confirmed are
 * always separate `state=` queries, never the combined default). */
export async function fetchInventoryPage(client: InventoryClient, state: InventoryTab, f: Filters, cursor?: string | null): Promise<Page> {
  return client.get<Page>(inventoryQuery(state, f, cursor));
}

/** Row rate assumed before the spectrum header has arrived — hk-pipeline's `spectrum_rows_per_s`,
 * and the same fallback `centre/live-spectrum.ts` uses for its own row period. */
export const DEFAULT_ROW_RATE_HZ = 25;

/** The state [[waterfallSpanS]], [[liveEdgeS]] and [[viewWindow]] read: geometry and the
 * capture clock, nothing measured. `AppState` satisfies it structurally. */
export interface WindowState {
  live: { view: { loHz: number; hiHz: number } | null; rowRateHz: number | null; edgeTS: number | null };
  device: { rowsPerS: number | null };
  time: { live: boolean; tS?: number; spanS?: number | null };
  captureWindow: { t0S: number; t1S: number; spanS: number } | null;
}

/**
 * The live edge, on the **capture clock**: the newest spectrum row's own time, else the capture
 * window's end, else `null` for *unknown* (T-379).
 *
 * `Date.now()` is not an answer and is never a fallback here. A replay, the mock SDR on a
 * time-compressed scene, and any source whose stamps are not the host's run on a clock of their
 * own; the fixture that exposed this sat 3.5 days from wall time, so a 20 s window ending at
 * browser-now selected nothing while five candidates stood in the store and four of them fell
 * inside the very same 20 s of capture. An empty list produced that way is
 * empty-because-not-fetched — the failure the whole-UI window rule names — and it is
 * indistinguishable on screen from a genuinely quiet band. Returning `null` makes the UI say
 * *unknown* instead of inventing a window.
 */
export function liveEdgeS(state: WindowState): number | null {
  return state.live.edgeTS ?? state.captureWindow?.t1S ?? null;
}

/** Seconds of time the waterfall is showing: its ring height over the row rate (≈ 20.5 s at the
 * default 25 rows/s). Pure arithmetic over already-known UI view state — which rows *qualify* in
 * that window is the backend's predicate, never this client's (§1 thin-client rule). */
export function waterfallSpanS(state: WindowState): number {
  const rate = state.live.rowRateHz ?? state.device.rowsPerS ?? DEFAULT_ROW_RATE_HZ;
  return WATERFALL_ROWS / Math.max(1e-3, rate);
}

/** The UI's one time window: `[t0, t1]` on the capture clock. */
export interface ViewWindow { t0: number; t1: number }

/**
 * The `[t0, t1]` the **Candidate** list is scoped to — **the window the waterfall is showing, and
 * nothing else** (ADR-0013 §3.3, ADR-0017 §2.1; CLAUDE.md's whole-UI window rule).
 *
 * This is deliberately the same arithmetic as the waterfall's own `historyWindow` (T-340): the span
 * the time navigator dragged when it asked for one, else the span the rows on screen cover. It used
 * to be a flat `REVIEW_WINDOW_S = 3600` while reviewing, which is a *second* window — the list then
 * answered about an hour while every other surface answered about the twenty seconds under it,
 * listing candidates that were nowhere on the waterfall and, once a span longer than an hour was
 * dragged, omitting ones that were. One window means one window.
 *
 * `null` is **unknown**: no live edge has been reported yet ([[liveEdgeS]]). The caller must then
 * say so rather than fall back to a window of its own — the whole point is that a fabricated window
 * renders as emptiness that looks exactly like a quiet band.
 */
export function viewWindow(state: WindowState): ViewWindow | null {
  const span = state.time.spanS !== undefined && state.time.spanS !== null && state.time.spanS > 0
    ? state.time.spanS
    : waterfallSpanS(state);
  if (!state.time.live && state.time.tS !== undefined) return { t0: state.time.tS - span, t1: state.time.tS };
  const edge = liveEdgeS(state);
  return edge === null ? null : { t0: edge - span, t1: edge };
}

/**
 * A string that changes exactly when [[viewWindow]] would (T-384): the time cursor, the stream's
 * live edge, the capture window's end and the row rate the span is derived from.
 *
 * Surfaces subscribe to *this*, not to `s.time` alone. A panel watching only the cursor never
 * re-reads when the live edge advances or a new capture window arrives, so it goes on answering
 * about the window it was mounted in while the waterfall beside it shows another — which is the
 * whole-UI window rule broken by omission rather than by a wrong query. One definition, so two
 * surfaces cannot disagree about when the window moved.
 */
export function windowKey(state: WindowState): string {
  const t = state.time;
  return [
    t.live ? "live" : `${t.tS}/${t.spanS ?? ""}`,
    state.live.edgeTS ?? "",
    state.captureWindow?.t1S ?? "",
    state.live.rowRateHz ?? "",
    state.device.rowsPerS ?? "",
  ].join("|");
}

/** The frequency filters both lists share — the tuned/zoomed view span. Deliberately carries no
 * time: the window belongs to the Candidate query alone (see [[loadInventoryRows]]). */
export function viewFilters(state: WindowState): Filters {
  const f: Filters = {};
  if (state.live.view) { f.fLoHz = state.live.view.loHz; f.fHiHz = state.live.view.hiHz; }
  return f;
}

/**
 * The filters the **Confirmed** list is queried with (T-263, ADR-0017 TM-7): [[viewFilters]] plus
 * `at` — the caller's own live edge — while scrubbed back.
 *
 * Still no `t0`/`t1`, and that is the whole point. A window *selects* rows, so windowing Confirmed
 * is exactly the regression §2.2 forbids: a catalogue entry that was quiet during the scrubbed
 * window would vanish. `at` carries the other half of what a window meant — it scopes the
 * `presence` projection and selects nothing — so the row stays listed and reads the liveness it had
 * *then* instead of the liveness it has now. Without it a scrubbed-back Confirmed list would mark
 * rows `live` from the wall clock while every other surface showed a past window.
 */
export function confirmedFilters(state: WindowState): Filters {
  const f = viewFilters(state);
  if (!state.time.live && state.time.tS !== undefined) f.at = state.time.tS;
  return f;
}

/** Loads both tabs' current pages and writes them into the store's `inventory.rows`. `onMore`
 * reports whether a tab's page was cut short (GAP 13 "500+" interim, §4.2).
 *
 * **Candidates are window-scoped; Confirmed are always listed** (ADR-0017 §2.2, invariant 3). A
 * Candidate is a hypothesis about energy in the window on screen, so outside that window there is
 * nothing to hypothesise about and the row simply isn't listed — no expiry timer and no decay. A
 * Confirmed row is a catalogue entry carrying its own presence track, so it stays listed whether or
 * not it is transmitting right now. Dropping that asymmetry would make a user's quiet confirmed
 * stations vanish from Explore the moment they went off the air.
 *
 * **Scrubbing back re-derives both lists** (T-263, TM-7) from the same two queries: the Candidate
 * window follows the scrubbed instant ([[viewWindow]]) and the Confirmed query names it as its
 * own live edge ([[confirmedFilters]]). Neither list is filtered here — the client chooses only
 * *which window to ask about*. */
export async function loadInventoryRows(ctx: AppContext, onMore: (tab: InventoryTab, more: boolean) => void): Promise<void> {
  const state = ctx.store.get();
  const f = viewFilters(state);
  const w = viewWindow(state);
  // T-379: no live edge reported yet — the window is *unknown*. Asking unwindowed would list
  // all-time candidates (widening the window, which breaks time-scoping) and asking on the
  // browser's clock would list none; both lie. So neither query is made up, and the list says
  // "unknown" instead of "nothing".
  if (!w) {
    ctx.store.set(setInventoryWindow(null));
    return;
  }
  const [confirmed, candidate] = await Promise.all([
    fetchInventoryPage(ctx.client, "confirmed", confirmedFilters(state)),
    fetchInventoryPage(ctx.client, "candidate", { ...f, t0: w.t0, t1: w.t1 }),
  ]);
  onMore("confirmed", !!confirmed.next_cursor);
  onMore("candidate", !!candidate.next_cursor);
  const rows: Record<string, Row> = {};
  for (const r of [...confirmed.entries, ...candidate.entries]) rows[r.id] = r;
  // The window's own coverage, asked for only when the list came back empty — the one case where
  // the difference between "nothing was on the air" and "nothing ever looked here" is the whole
  // message. It is the backend's `Coverage` (T-368), read as served; nothing here decides it.
  const coverage = candidate.entries.length === 0 ? await windowCoverage(ctx.client, state, w) : "observed";
  ctx.store.set(setInventoryRows(rows, Date.now() / 1000));
  ctx.store.set(setInventoryWindow({ t0: w.t0, t1: w.t1, coverage }));
}

/** What the current window's frequency range is, for a coverage question about exactly it. */
function windowBand(state: WindowState): { lo: number; hi: number } | null {
  const v = state.live.view;
  return v && v.hiHz > v.loHz ? { lo: v.loHz, hi: v.hiHz } : null;
}

/**
 * Whether the front end ever sampled this (time × frequency) window, straight from
 * `GET /api/coverage` (T-368): `"observed"`, `"unobserved"`, or `null` for *not known*.
 *
 * The route's own rule is `grey a cell if and only if its state is "unobserved"`, and its
 * `Coverage::of` refuses to mint an observation out of a zero span — so there is no parallel notion
 * of emptiness invented here. One cell, because the question is about the window as a whole: it is
 * unobserved only when **no** part of the band was sampled in it.
 *
 * Shared with the output/decode panels (T-384) rather than re-asked there: two surfaces that
 * decided "unobserved" by two routines would eventually disagree about one window, and the rule
 * they are both held to is that emptiness names a single cause.
 */
export async function windowCoverage(
  client: Pick<InventoryClient, "get">, state: WindowState, w: { t0: number; t1: number },
): Promise<WindowCoverage> {
  const band = windowBand(state);
  if (!band || !(w.t1 > w.t0)) return null;
  try {
    const body = await client.get<CoverageResponse>(
      `/api/coverage?f_lo=${band.lo}&f_hi=${band.hi}&cells=1&t0=${w.t0}&t1=${w.t1}`,
    );
    const cells = surveyCells(body);
    // Not asked about, or served nothing: unknown. Never "unobserved" by default — that would be a
    // measurement claim made out of a missing answer.
    return cells.length === 0 ? null : cells.some((c) => c.state === "observed") ? "observed" : "unobserved";
  } catch {
    return null;
  }
}

// ---- user band (T-191 route, T-193 draggable box edges) ----

/** The client surface [[setUserBand]]/[[clearUserBand]] need. */
export interface BandClient { put<T = unknown>(path: string, body: unknown): Promise<T>; del<T = unknown>(path: string): Promise<T> }

export type BandResult = { ok: true; entry: Row } | { ok: false; message: string };

/** Sets (replaces) the user band override on `id` — a user's drag of a Confirmed signal's box
 * edges, committed on release. `docs/api.md PUT /api/inventory/{id}/band`; `400 invalid` (e.g. too
 * far from the measured band, or over the max width) reports the server's own message, never a
 * client-invented one. */
export async function setUserBand(client: BandClient, id: string, fLo: number, fHi: number): Promise<BandResult> {
  try {
    const r = await client.put<{ user_band: UserBand; entry: Row }>(`/api/inventory/${encodeURIComponent(id)}/band`, { f_lo: fLo, f_hi: fHi });
    return { ok: true, entry: r.entry };
  } catch (e) {
    return { ok: false, message: apiErrorText(e) };
  }
}

/** Clears the user band override, back to the measured band ("Reset band", the context menu).
 * `docs/api.md DELETE /api/inventory/{id}/band`. */
export async function clearUserBand(client: BandClient, id: string): Promise<BandResult> {
  try {
    const r = await client.del<{ cleared: boolean; entry: Row }>(`/api/inventory/${encodeURIComponent(id)}/band`);
    return { ok: true, entry: r.entry };
  } catch (e) {
    return { ok: false, message: apiErrorText(e) };
  }
}

// ---- sort ----

const SORT_VALUE: Record<InventorySortKey, (r: Row) => number> = {
  freq: (r) => r.f_center_hz,
  last_seen: (r) => r.last_seen_s,
  count: (r) => r.count,
  bandwidth: (r) => r.bandwidth_hz,
};
const DEFAULT_DIR: Partial<Record<InventorySortKey, 1 | -1>> = { count: -1, last_seen: -1 };

/** Click-to-sort transition (same rule as the old page's `nextSort`, T-148): clicking the active
 * column flips its direction; a different column switches to its own default direction. */
export function nextInventorySort(current: { key: InventorySortKey; dir: 1 | -1 }, key: InventorySortKey): { key: InventorySortKey; dir: 1 | -1 } {
  return { key, dir: current.key === key ? ((-current.dir) as 1 | -1) : (DEFAULT_DIR[key] ?? 1) };
}

/** Sorts rows by `key`/`dir`; pure, unit-tested without a DOM. */
export function sortInventoryRows(rows: readonly Row[], key: InventorySortKey, dir: 1 | -1): Row[] {
  const f = SORT_VALUE[key];
  return [...rows].sort((a, b) => (f(a) - f(b)) * dir);
}

// ---- the empty state ----

/** The state [[emptyListText]] reads. `AppState["inventory"]` satisfies it structurally. */
export interface EmptyState { window: { coverage: WindowCoverage } | null; loadedAtS: number | null; error: string | null }

/**
 * What an empty Candidate/Confirmed list says, and it must never be one sentence (T-379).
 *
 * The whole-UI window rule permits a surface to render empty **only where data genuinely does not
 * exist**, which makes the two emptinesses different claims:
 *
 * | state | what it means | sentence |
 * |---|---|---|
 * | no window known | the UI does not know which window to ask about | "Waiting for the capture window…" |
 * | window unobserved | nothing ever looked here | "Nothing was observed in this window — no data, not a quiet band." |
 * | window observed | the receiver listened and heard nothing | "Nothing on the air in this window." |
 * | coverage unknown | the window was asked about; whether it was sampled is not known | "Nothing listed for this window." |
 *
 * Only the third is a finding. Rendering the first or second as the third invents an
 * absence-of-signal result out of an absence of measurement — the same error the waterfall's grey
 * rule exists to stop, on this surface.
 */
export function emptyListText(s: EmptyState): string {
  if (s.error) return s.error;
  if (s.window === null) return s.loadedAtS === null ? "Loading…" : "Waiting for the capture window…";
  if (s.window.coverage === "unobserved") return "Nothing was observed in this window — no data, not a quiet band.";
  if (s.window.coverage === "observed") return "Nothing on the air in this window.";
  return "Nothing listed for this window.";
}

// ---- row view model ----

export interface Chip { cls: "known" | "unknown" | "flag" | "cluster"; text: string }

/** The family/flag chip(s) for a row, from already-known fields only (`family`,
 * `classification.family`, `explanations[0].flags`); "unknown" when no family is known yet. */
export function rowChips(r: Row): Chip[] {
  const family = r.family ?? r.classification?.family ?? null;
  const chips: Chip[] = [{ cls: family ? "known" : "unknown", text: family ?? "unknown" }];
  const flags = r.explanations[0]?.flags ?? [];
  if (flags.includes("off-raster")) chips.push({ cls: "flag", text: "off raster" });
  else if (flags.includes("off-allocation")) chips.push({ cls: "flag", text: "off allocation" });
  return chips;
}

/** A "seen before" chip when the row currently belongs to a visible cluster (`cluster_id`, T-202):
 * evidence that this emission *measures* like something seen before, never an identity or a
 * family (ADR-0016 §5 "a cluster is a type, an emitter is an instance") — kept a distinct `cls`
 * from `rowChips`' family/flag chips so it never reads as either. `null` without a cluster. */
export function clusterChip(r: Pick<Row, "cluster_id">): Chip | null {
  return r.cluster_id ? { cls: "cluster", text: "seen before" } : null;
}

/** "Seen" text for a row (§4.2): confirmed rows show on-air duty and count (GAP 2 interim — no
 * `snr_db`/`peak_dbfs` on the row, so no level bar); candidates show a recurrence rate. Both read
 * only `recurrence`/`count`, never invent a rate the server didn't report. */
export function rowSeenText(r: Row): string {
  const rec = r.recurrence;
  if (!rec) return `${r.count} seen`;
  if (r.state === "confirmed") return `${Math.round(rec.duty_cycle * 100)}% on-air · ${r.count} seen`;
  if (rec.span_s > 0) {
    const perHour = rec.occurrences / (rec.span_s / 3600);
    return perHour >= 1
      ? `${perHour < 10 ? perHour.toFixed(1) : Math.round(perHour)}×/h`
      : `${rec.occurrences}× in ${Math.max(1, Math.round(rec.span_s / 60))} min`;
  }
  return `${rec.occurrences}×`;
}

/** A sparkline of `recurrence.recent[]` counts, normalised 0–1, most recent last; empty without
 * recent history yet. Candidate rows show this as the mockup's activity dots. */
export function recurrenceDots(r: Row, n = 8): number[] {
  const recent = r.recurrence?.recent ?? [];
  if (recent.length === 0) return [];
  const tail = recent.slice(-n);
  const max = Math.max(1, ...tail.map((a) => a.count));
  return tail.map((a) => a.count / max);
}
