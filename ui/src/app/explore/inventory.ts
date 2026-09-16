// Inventory row model, queries and actions (ADR-0013 §4.2, §4.5, T-151). Reuses the old page's
// pure helpers (query building, promote/delete, T-080's per-state split) where their shape still
// fits, and adds the richer row fields docs/api.md `/api/inventory` serves — `classification`,
// `explanations`, `refined` — that the old page's `Row` never needed.
import { deleteEntry, inventoryQuery, promoteEntry, rowListenTarget, type ActionResult, type Filters, type InventoryClient, type Recurrence as BaseRecurrence, type Row as BaseRow, type UserBand } from "../../inventory";
import { WATERFALL_ROWS } from "../../waterfall";
import type { AppContext } from "../context";
import { apiErrorText } from "./format";
import { setInventoryRows } from "./slice";
import type { InventoryTab, InventorySortKey } from "./slice";

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

/** How long the review-mode inventory query looks back from the reviewed instant (§3.3: the
 * inventory adds `t0`/`t1` while reviewing; no fixed window is specified, so this picks one hour of
 * context, matching the capture band's default scale). */
export const REVIEW_WINDOW_S = 3600;

/** Row rate assumed before the spectrum header has arrived — hk-pipeline's `spectrum_rows_per_s`,
 * and the same fallback `centre/live-spectrum.ts` uses for its own row period. */
export const DEFAULT_ROW_RATE_HZ = 25;

/** The state [[waterfallSpanS]] and [[candidateWindow]] read: geometry and clock, nothing measured.
 * `AppState` satisfies it structurally. */
export interface WindowState {
  live: { view: { loHz: number; hiHz: number } | null; rowRateHz: number | null };
  device: { rowsPerS: number | null };
  time: { live: boolean; tS?: number };
}

/** Seconds of time the waterfall is showing: its ring height over the row rate (≈ 20.5 s at the
 * default 25 rows/s). Pure arithmetic over already-known UI view state — which rows *qualify* in
 * that window is the backend's predicate, never this client's (§1 thin-client rule). */
export function waterfallSpanS(state: WindowState): number {
  const rate = state.live.rowRateHz ?? state.device.rowsPerS ?? DEFAULT_ROW_RATE_HZ;
  return WATERFALL_ROWS / Math.max(1e-3, rate);
}

/** The `[t0, t1]` the **Candidate** list is scoped to: the span the waterfall shows, ending at the
 * live edge — or at the reviewed instant, looking back [[REVIEW_WINDOW_S]], when scrubbed back
 * (ADR-0013 §3.3, ADR-0017 §2.1). */
export function candidateWindow(state: WindowState, nowS: number): { t0: number; t1: number } {
  if (!state.time.live && state.time.tS !== undefined) return { t0: state.time.tS - REVIEW_WINDOW_S, t1: state.time.tS };
  return { t0: nowS - waterfallSpanS(state), t1: nowS };
}

/** The frequency filters both lists share — the tuned/zoomed view span. Deliberately carries no
 * time: the window belongs to the Candidate query alone (see [[loadInventoryRows]]). */
export function viewFilters(state: WindowState): Filters {
  const f: Filters = {};
  if (state.live.view) { f.fLoHz = state.live.view.loHz; f.fHiHz = state.live.view.hiHz; }
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
 * stations vanish from Explore the moment they went off the air. */
export async function loadInventoryRows(ctx: AppContext, onMore: (tab: InventoryTab, more: boolean) => void, nowS: number = Date.now() / 1000): Promise<void> {
  const state = ctx.store.get();
  const f = viewFilters(state);
  const w = candidateWindow(state, nowS);
  const [confirmed, candidate] = await Promise.all([
    fetchInventoryPage(ctx.client, "confirmed", f),
    fetchInventoryPage(ctx.client, "candidate", { ...f, t0: w.t0, t1: w.t1 }),
  ]);
  onMore("confirmed", !!confirmed.next_cursor);
  onMore("candidate", !!candidate.next_cursor);
  const rows: Record<string, Row> = {};
  for (const r of [...confirmed.entries, ...candidate.entries]) rows[r.id] = r;
  ctx.store.set(setInventoryRows(rows, Date.now() / 1000));
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
