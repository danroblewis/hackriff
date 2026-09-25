// Signal inventory: two lists, candidates and confirmed, over /api/inventory?state=... (T-018
// query; T-080 split + promote/delete). Identity gating is done by the server; this only renders
// what it returns (a withheld identity has no value to show). Text goes in via textContent only.
export type EntryState = "candidate" | "confirmed" | "deleted";

/** hk-model recurrence stats (docs/api.md `GET /api/inventory`); shown as the API's own numbers,
 * with no client-side interpretation. */
export interface Recurrence { occurrences: number; appearances: number; span_s: number; on_air_s: number; duty_cycle: number }

/** A user's band-edge override (docs/api.md `user_band`, T-191/T-193): edges in Hz, stored beside
 * the measured `f_lo_hz`/`f_hi_hz`, which it never overwrites. `null` on the row when unset. */
export interface UserBand { f_lo: number; f_hi: number; set_at: number; actor: string; reason: string | null; reason_withheld: boolean }

/** The latest presence interval intersecting the request's window (docs/api.md `presence.last_interval`,
 * T-284, ADR-0017 TM-2/§2.3): "the box the waterfall draws" (TM-4). `open`: still on the air at the
 * window's own live edge — an advancing `t_end_s` on the next poll is what makes a box grow; nothing
 * here is extrapolated client-side between polls. */
/** `revoked_s` (T-413, ADR-0019 §6.1) is measured silence *inside* this interval whose detected end
 * a resumption revoked — the interval is one interval on one emitter, and this says how much of the
 * span it covers the receiver measured empty. Absent on a pre-T-413 backend; `0` for the
 * overwhelming majority of intervals. Nothing in the client computes air time from it: it is why a
 * rejoined box is marked rather than drawn as if it were on air throughout. */
export interface PresenceInterval { t_start_s: number; t_end_s: number; open: boolean; revoked_s?: number }

/** When this emitter was on the air, seen through the request's window (docs/api.md `presence`,
 * T-284, ADR-0017 TM-2/§2.3) — read this instead of `first_seen_s`/`last_seen_s`, which are the
 * *hull* of the presence track and never its extent. `last_interval` is `null` when no interval
 * intersects the window: render nothing for it, never a fabricated zero-width/zero-duration box. */
export interface Presence {
  intervals: number; on_air_s: number; last_interval: PresenceInterval | null;
  liveness: "live" | "ended" | "absent"; ended_t_s: number | null;
}

/** T-219/T-587: why this row currently defers to another row — a Confirmed overlap, the weaker of
 * a duplicate pair, or a receiver artifact (docs/api.md `relation`). `artifact` names the mechanism
 * for `artifact-of` (`image`/`harmonic`/`intermod`); `null` for every other kind, including
 * `retune-sibling-of` (T-598), whose own mechanism lives in `detail.slope`. `reason` is always
 * backend-rendered prose — "image of the 100.8 MHz carrier" — and is the only source of that text;
 * nothing client-side re-derives or paraphrases it. */
export interface Relation {
  kind: "suppressed-by" | "duplicate-of" | "artifact-of" | "retune-sibling-of";
  artifact: "image" | "harmonic" | "intermod" | null;
  source_id: string;
  author: "system" | "user";
  actor: string;
  t_s: number;
  reason: string;
  score: number | null;
  detail: Record<string, unknown> | null;
}

export interface Row {
  id: string; state: EntryState;
  f_center_hz: number; bandwidth_hz: number; f_lo_hz: number; f_hi_hz: number;
  first_seen_s: number; last_seen_s: number; count: number;
  known_status: "known" | "unexpected-here" | "unknown";
  status: { author: string; reason: string | null; prior_ref: string | null; reason_withheld: boolean } | null;
  tags: string[]; tags_withheld?: boolean; family: string | null;
  identity_scheme: string | null; identity_value?: string; identity_class: string | null; withheld: boolean;
  /** docs/api.md `identity_label`/`identity_confidence` (T-967): the backend's own human-readable
   * rendering of the decoded identity (RDS's PS station name, and similarly-named fields from
   * other decoders) and the decoder's own confidence/vote share behind it. Both `null` without a
   * recognised field, and on a withheld row — never present without `identity_scheme`. Optional
   * so fixtures/tests that predate T-967 still typecheck; a current server always sends both. */
  identity_label?: string | null; identity_confidence?: number | null;
  recurrence: Recurrence | null;
  /** Optional so existing fixtures/tests that predate T-193 still typecheck; a server that serves
   * the field always sends `null` when unset, never omits it. */
  user_band?: UserBand | null;
  /** Optional so fixtures/tests that predate T-284 still typecheck; a live server serves it on
   * every row, windowed or not (docs/api.md). `undefined` is treated exactly like a row with no
   * interval in the window — no box drawn — never a fabricated one. */
  presence?: Presence;
  /** Optional for the same reason `presence` is: a server predating this field, or a fixture built
   * before it, still typechecks. A current server always sends `null` on a row with no standing
   * relation — never omits the key — so `undefined` here is treated exactly like `null`. */
  relation?: Relation | null;
}
interface Page { entries: Row[]; next_cursor: string | null }
type Key = "status" | "freq" | "bw" | "family" | "identity" | "count" | "recurrence" | "tags";

const STATUS_ORDER = { "unexpected-here": 0, unknown: 1, known: 2 };
const identityText = (r: Row) => r.identity_value ?? (r.withheld ? "withheld" : "");

/** What a row's Listen affordance (its own button, or the row click's inspect target) points
 * Listen at: this emitter, by id (T-069). Pure so it's unit-tested without a DOM. */
export function rowListenTarget(r: Pick<Row, "id" | "f_center_hz">): { emitter: string; label: string } {
  return { emitter: r.id, label: `${(r.f_center_hz / 1e6).toFixed(4)} MHz` };
}

/** A sort key's value for one row; `null` means the field is absent, which [[sortRows]] always
 * places last regardless of direction. */
const sortValue: Record<Key, (r: Row) => number | string | null> = {
  status: (r) => STATUS_ORDER[r.known_status] ?? 3,
  freq: (r) => r.f_center_hz, bw: (r) => r.bandwidth_hz, family: (r) => r.family ?? "",
  identity: (r) => `${r.identity_scheme ?? ""} ${identityText(r)}`,
  count: (r) => r.count, recurrence: (r) => r.recurrence?.occurrences ?? null,
  tags: (r) => r.tags.join(","),
};

export interface SortState { key: Key; dir: 1 | -1 }

/** First-click direction per column: busiest/most-recurring first for count and recurrence,
 * ascending for everything else (frequency low-to-high, text A-to-Z). */
const DEFAULT_DIR: Partial<Record<Key, 1 | -1>> = { count: -1, recurrence: -1 };

/** Click-to-sort transition: clicking the already-active column toggles its direction; clicking a
 * different column switches to it at that column's default direction. Pure so header-click
 * behaviour is unit-tested without a DOM. */
export function nextSort(current: SortState, key: Key): SortState {
  return { key, dir: current.key === key ? ((-current.dir) as 1 | -1) : (DEFAULT_DIR[key] ?? 1) };
}

/** Sorts rows by `key`/`dir`. A row missing the field (`null`) always sorts last, in either
 * direction. Pure so ordering is unit-tested without a DOM. */
export function sortRows(rows: readonly Row[], key: Key, dir: 1 | -1): Row[] {
  const f = sortValue[key];
  return [...rows].sort((a, b) => {
    const x = f(a), y = f(b);
    if (x === null || y === null) return x === y ? 0 : x === null ? 1 : -1;
    return (x < y ? -1 : x > y ? 1 : 0) * dir;
  });
}

/** Shared filter fields (frequency, time, status, tag): applied to both the candidate and
 * confirmed queries alike. */
export interface Filters {
  fLoHz?: number; fHiHz?: number; t0?: number; t1?: number;
  /** The caller's own live edge for a query with **no** `t0`/`t1` (docs/api.md `/api/inventory`
   * `at`, T-263): it scopes each row's `presence` projection and selects nothing, so a scrubbed-back
   * list re-derives the liveness its rows had then without being time-filtered. */
  at?: number;
  status?: string; tag?: string;
  /** `relations=` (docs/api.md, T-219): `shown` (default) hides a row with a standing relation
   * claim entirely; `all` lists it too, with `relation` set so the caller can render it. T-587
   * asks for `all` on the Explore queries — an artefact the backend has already explained must
   * reach the screen labelled, not vanish the way `shown` was hiding it. */
  relations?: "shown" | "all";
}

/** The `/api/inventory` query for one state's list (T-080: candidates and confirmed load
 * separately — `state=candidate` / `state=confirmed` — never the default combined page). */
export function inventoryQuery(state: "candidate" | "confirmed", f: Filters, cursor?: string | null): string {
  const p = new URLSearchParams();
  p.set("state", state);
  if (f.fLoHz !== undefined) p.set("f_lo", String(f.fLoHz));
  if (f.fHiHz !== undefined) p.set("f_hi", String(f.fHiHz));
  if (f.t0 !== undefined) p.set("t0", String(f.t0));
  if (f.t1 !== undefined) p.set("t1", String(f.t1));
  if (f.at !== undefined) p.set("at", String(f.at));
  if (f.status) p.set("status", f.status);
  if (f.tag) p.set("tag", f.tag);
  if (f.relations) p.set("relations", f.relations);
  p.set("limit", "200");
  if (cursor) p.set("cursor", cursor);
  return `/api/inventory?${p}`;
}

/** Minimal server surface the table needs: list rows, promote a candidate, delete an entry — all
 * Bearer-authenticated by the client (controls/client.ts `ControlClient`, which this matches). */
export interface InventoryClient {
  get<T = unknown>(path: string): Promise<T>;
  post<T = unknown>(path: string, body?: unknown): Promise<T>;
  del<T = unknown>(path: string): Promise<T>;
}

/** Loads one page for one state. Pure aside from the client call, so the split-by-state behaviour
 * is unit-tested without a DOM. */
export async function loadInventoryPage(client: InventoryClient, state: "candidate" | "confirmed", f: Filters, cursor?: string | null): Promise<Page> {
  return client.get<Page>(inventoryQuery(state, f, cursor));
}

/** The server's `{"error", "code"}` body (docs/api.md), shown verbatim; falls back to a plain message. */
function errText(e: unknown): string {
  const anyE = e as { code?: unknown; message?: unknown };
  const code = anyE && typeof anyE.code === "string" ? anyE.code : null;
  const msg = e instanceof Error ? e.message : String(e);
  return code ? `${msg} (${code})` : msg;
}

export type ActionResult = { ok: true } | { ok: false; message: string };

/** Promotes a candidate to confirmed, then reloads (T-080). Reload runs only on success: a
 * refused promote changes nothing server-side, so the error stays on screen instead of being
 * overwritten by a reload. */
export async function promoteEntry(client: InventoryClient, id: string, reload: () => Promise<void>): Promise<ActionResult> {
  try {
    await client.post(`/api/inventory/${encodeURIComponent(id)}/promote`);
    await reload();
    return { ok: true };
  } catch (e) {
    return { ok: false, message: errText(e) };
  }
}

/** Deletes an entry, then reloads (T-080). Same reload-on-success-only rule as [[promoteEntry]]. */
export async function deleteEntry(client: InventoryClient, id: string, reload: () => Promise<void>): Promise<ActionResult> {
  try {
    await client.del(`/api/inventory/${encodeURIComponent(id)}`);
    await reload();
    return { ok: true };
  } catch (e) {
    return { ok: false, message: errText(e) };
  }
}

