// Region selections (T-044 client model, T-052 server-backed). Several exist at once; each is a
// first-class object (hk-model `Selection`, docs/07 §2.20) with an id, a name, a frequency extent,
// optionally a time extent, notes, tags and links to the actions taken on it. Pure (no DOM):
// unit-tested in ui/test/selections.test.ts.
//
// Sync model:
// - **Optimistic.** add/rename/remove/link change the list at once, then go to the server
//   (`/api/selections`, crates/hk-api/src/selections.rs) one operation at a time.
// - **Client ids.** A selection's id is a UUID chosen here, so a selection made offline keeps its
//   identity: create answers 409 when it already exists and the store updates instead; update
//   answers 404 when it does not and the store creates it.
// - **Offline fallback.** A network failure, 401 or 5xx switches to offline mode: nothing is lost,
//   changes stay in this page as pending, `sync()` says so, and `flush()` (retried by main.ts)
//   sends them when the server is back. A 4xx refusal rolls the change back with a notice.
// - **No backend** (tests, or a page without a token): client-only, like T-044.
import { ControlError, reactionTo } from "./controls/client";

export type LinkKind = "demodulation" | "recording" | "bitstream" | "inspection";

/** An action taken on a selection (hk-model `SelectionLink`). `t` in Unix seconds. */
export interface SelectionLink { kind: LinkKind; target: string; t: number; note: string | null }

/** What an action hook reports to record as a link. */
export interface NewLink { kind: LinkKind; target: string; note?: string }

/** One selected region. Frequencies in Hz; times in Unix seconds (floats), like the API. */
export interface Selection {
  id: string;
  name: string;
  f_lo: number;
  f_hi: number;
  t_lo?: number;
  t_hi?: number;
  notes?: string;
  tags: string[];
  links: SelectionLink[];
  /** When it was made / last changed, Unix seconds. */
  created: number;
  updated: number;
}

/** What a drag (or a caller) supplies; id, times and a default name are filled in. */
export interface NewSelection {
  name?: string;
  f_lo: number;
  f_hi: number;
  t_lo?: number;
  t_hi?: number;
}

/** Editable fields. */
export interface SelectionPatch { name?: string; notes?: string | null; tags?: string[] }

/** Actions each selection offers (rendered by selection-panel.ts, dispatched by `runSelectionAction`). */
export const SELECTION_ACTIONS = [
  { id: "inspect", label: "Inspect", title: "Region history plus the ranked explanations of the emitters inside" },
  { id: "listen", label: "Listen", title: "Demodulate the strongest signal in this region to audio (mode estimated, T-043)" },
  { id: "demod", label: "Demod", title: "Demodulated outputs per selection (bits, symbols, WAV: T-061)" },
  { id: "record", label: "Record", title: "Record this region (the tuned window now; band-limited outputs in T-061)" },
] as const;
export type ActionId = (typeof SELECTION_ACTIONS)[number]["id"];

export const MAX_NAME_LEN = 64;
/** hk-model `SELECTION_LINKS_MAX`. */
export const MAX_LINKS = 256;

/** Why `s` is not a valid selection, or null. */
export function validateSelection(s: NewSelection): string | null {
  if (!Number.isFinite(s.f_lo) || !Number.isFinite(s.f_hi)) return "frequencies must be finite";
  if (s.f_lo < 0 || !(s.f_hi > s.f_lo)) return "need 0 <= f_lo < f_hi";
  if ((s.t_lo === undefined) !== (s.t_hi === undefined)) return "set both t_lo and t_hi, or neither";
  if (s.t_lo !== undefined && (!Number.isFinite(s.t_lo) || !Number.isFinite(s.t_hi!) || s.t_hi! < s.t_lo)) {
    return "need finite t_lo <= t_hi";
  }
  if (s.name !== undefined && cleanName(s.name) === "") return "name must not be empty";
  return null;
}

const cleanName = (name: string) => Array.from(name.replace(/\s+/g, " ").trim()).slice(0, MAX_NAME_LEN).join("");

/** A random v4 UUID (getRandomValues works on plain-http LAN pages, randomUUID does not). */
export function uuid4(): string {
  const b = new Uint8Array(16);
  crypto.getRandomValues(b);
  b[6] = (b[6] & 0x0f) | 0x40;
  b[8] = (b[8] & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

// ---- server transport ----

/** The server operations the store needs. */
export interface SelectionBackend {
  list(): Promise<Selection[]>;
  create(s: Selection): Promise<Selection>;
  update(id: string, s: Selection): Promise<Selection>;
  remove(id: string): Promise<void>;
  link(id: string, link: NewLink): Promise<Selection>;
}

/** The JSON a server selection arrives as (nulls for absent optionals). */
interface Wire {
  id: string; name: string; f_lo: number; f_hi: number; t_lo: number | null; t_hi: number | null;
  notes: string | null; tags: string[]; links: SelectionLink[]; created: number; updated: number;
}

/** A server selection as the store keeps it (absent optionals omitted). */
export function fromWire(w: Wire): Selection {
  const s: Selection = { id: w.id, name: w.name, f_lo: w.f_lo, f_hi: w.f_hi, tags: w.tags ?? [], links: w.links ?? [], created: w.created, updated: w.updated };
  if (w.t_lo !== null && w.t_hi !== null && w.t_lo !== undefined && w.t_hi !== undefined) { s.t_lo = w.t_lo; s.t_hi = w.t_hi; }
  if (w.notes) s.notes = w.notes;
  return s;
}

const enc = encodeURIComponent;
const body = (s: Selection) => ({
  name: s.name, f_lo: s.f_lo, f_hi: s.f_hi, t_lo: s.t_lo ?? null, t_hi: s.t_hi ?? null, notes: s.notes ?? null, tags: s.tags,
});

/** Minimal client surface (controls/client.ts `ControlClient`). */
export interface ApiClient {
  get<T>(path: string): Promise<T>;
  post<T>(path: string, body?: unknown): Promise<T>;
  put<T>(path: string, body: unknown): Promise<T>;
  del<T>(path: string): Promise<T>;
}

/** `/api/selections` over the token-carrying client (Bearer header on every call). */
export function apiBackend(client: ApiClient): SelectionBackend {
  return {
    list: async () => (await client.get<{ selections: Wire[] }>("/api/selections")).selections.map(fromWire),
    create: async (s) => {
      const b: Record<string, unknown> = { id: s.id, ...body(s) };
      if (s.t_lo === undefined) { delete b.t_lo; delete b.t_hi; }
      return fromWire(await client.post<Wire>("/api/selections", b));
    },
    update: async (id, s) => fromWire(await client.put<Wire>(`/api/selections/${enc(id)}`, body(s))),
    remove: async (id) => { await client.del(`/api/selections/${enc(id)}`); },
    link: async (id, l) => fromWire(await client.post<Wire>(`/api/selections/${enc(id)}/links`, l)),
  };
}

const statusOf = (e: unknown) => (e instanceof ControlError ? e.status : 0);
/** Failures that keep changes pending (network, token, server trouble) rather than rolling back. */
const keepsPending = (e: unknown) => { const st = statusOf(e); return st === 0 || st === 401 || st === 408 || st >= 500; };
const errText = (e: unknown) => (e instanceof ControlError ? `${e.status} ${e.message}` : e instanceof Error ? e.message : String(e));

// ---- the store ----

export type SyncMode = "local" | "syncing" | "synced" | "offline";
export interface SyncState { mode: SyncMode; pending: number; message: string }

export interface StoreOptions {
  /** Clock, Unix seconds (tests inject one). */
  now?: () => number;
  /** Id generator (tests inject one; must be a UUID with a real server). */
  newId?: () => string;
  /** Server transport; without one the store is client-only. */
  backend?: SelectionBackend | null;
}

/** The selections of this page, in creation order, synced with the server when it has a backend. */
export class SelectionStore {
  private items: Selection[] = [];
  private listeners = new Set<(list: readonly Selection[]) => void>();
  private counter = 0;
  private now: () => number;
  private newId: () => string;
  private backend: SelectionBackend | null;
  private mode: SyncMode;
  private message = "";
  /** Ids that need an upsert, ids deleted locally, and links not yet on the server. */
  private dirty = new Set<string>();
  private deleted = new Set<string>();
  private links = new Map<string, NewLink[]>();
  /** Last copy the server confirmed (rollback target). */
  private server = new Map<string, Selection>();
  private picked = new Set<string>();
  private chain: Promise<void> = Promise.resolve();

  constructor(opts: StoreOptions = {}) {
    this.now = opts.now ?? (() => Date.now() / 1000);
    this.newId = opts.newId ?? uuid4;
    this.backend = opts.backend ?? null;
    this.mode = this.backend ? "syncing" : "local";
  }

  list(): readonly Selection[] {
    return this.items;
  }

  get(id: string): Selection | undefined {
    return this.items.find((s) => s.id === id);
  }

  /** Sync mode, pending change count and the latest notice. */
  sync(): SyncState {
    const ids = new Set([...this.dirty, ...this.deleted, ...this.links.keys()]);
    return { mode: this.mode, pending: ids.size, message: this.message };
  }

  /** Adds a selection; throws on an invalid one (see [`validateSelection`]). */
  add(s: NewSelection): Selection {
    const problem = validateSelection(s);
    if (problem) throw new Error(problem);
    this.counter++;
    const t = this.now();
    const sel: Selection = {
      id: this.newId(),
      name: s.name !== undefined ? cleanName(s.name) : `Region ${this.counter}`,
      f_lo: s.f_lo,
      f_hi: s.f_hi,
      tags: [],
      links: [],
      created: t,
      updated: t,
    };
    if (s.t_lo !== undefined) { sel.t_lo = s.t_lo; sel.t_hi = s.t_hi; }
    this.items = [...this.items, sel];
    if (this.backend) this.dirty.add(sel.id);
    this.emit();
    this.push(sel.id);
    return sel;
  }

  /** Renames; false when the id is unknown or the name is empty after trimming. */
  rename(id: string, name: string): boolean {
    const clean = cleanName(name);
    if (clean === "") return false;
    return this.edit(id, { name: clean });
  }

  /** Edits name, notes or tags; false when the id is unknown or the patch is invalid. */
  edit(id: string, patch: SelectionPatch): boolean {
    const cur = this.get(id);
    if (!cur) return false;
    const next: Selection = { ...cur, updated: Math.max(cur.created, this.now()) };
    if (patch.name !== undefined) {
      const n = cleanName(patch.name);
      if (!n) return false;
      next.name = n;
    }
    if (patch.notes !== undefined) { if (patch.notes) next.notes = patch.notes; else delete next.notes; }
    if (patch.tags !== undefined) next.tags = [...new Set(patch.tags.map((x) => x.trim()).filter(Boolean))];
    this.replace(next);
    if (this.backend) this.dirty.add(id);
    this.emit();
    this.push(id);
    return true;
  }

  remove(id: string): boolean {
    const before = this.items.length;
    this.items = this.items.filter((s) => s.id !== id);
    if (this.items.length === before) return false;
    this.picked.delete(id);
    this.dirty.delete(id);
    this.links.delete(id);
    if (this.backend) this.deleted.add(id);
    this.emit();
    this.push(id);
    return true;
  }

  clear() {
    for (const s of [...this.items]) this.remove(s.id);
  }

  /** Records an action taken on a selection (optimistic; sent to the server). */
  link(id: string, link: NewLink): boolean {
    const cur = this.get(id);
    if (!cur) return false;
    const l: SelectionLink = { kind: link.kind, target: link.target, t: this.now(), note: link.note ?? null };
    this.replace({ ...cur, links: [...cur.links, l].slice(-MAX_LINKS) });
    if (this.backend) this.links.set(id, [...(this.links.get(id) ?? []), link]);
    this.emit();
    this.push(id);
    return true;
  }

  // ---- multi-select ----

  isPicked(id: string): boolean { return this.picked.has(id); }
  /** Selections ticked for a bulk action, in list order. */
  pickedList(): Selection[] { return this.items.filter((s) => this.picked.has(s.id)); }
  pick(id: string, on = !this.picked.has(id)) {
    if (!this.get(id)) return;
    if (on) this.picked.add(id); else this.picked.delete(id);
    this.emit();
  }
  pickAll(on: boolean) {
    this.picked = on ? new Set(this.items.map((s) => s.id)) : new Set();
    this.emit();
  }

  /** Calls `fn` after every change; returns an unsubscribe function. */
  subscribe(fn: (list: readonly Selection[]) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  // ---- server sync ----

  /** Loads the server's selections, keeping local changes not yet synced, then flushes them. */
  async load(): Promise<void> {
    const b = this.backend;
    if (!b) return;
    await this.enqueue(async () => {
      let remote: Selection[];
      try {
        remote = await b.list();
      } catch (e) {
        this.offline(e, "server selections unavailable");
        return;
      }
      this.server = new Map(remote.map((s) => [s.id, s]));
      const local = new Map(this.items.map((s) => [s.id, s]));
      const merged = remote.filter((s) => !this.deleted.has(s.id)).map((s) => (this.dirty.has(s.id) || this.links.has(s.id) ? local.get(s.id) ?? s : s));
      for (const s of this.items) if (!this.server.has(s.id)) merged.push(s); // made here, not yet on the server
      merged.sort((a, c) => a.created - c.created);
      this.items = merged;
      this.picked = new Set([...this.picked].filter((id) => local.has(id) || this.server.has(id)));
      this.mode = "synced";
      this.message = "";
      this.emit();
      await this.sendAll();
    });
  }

  /** Retries pending changes (main.ts calls it while offline). */
  async flush(): Promise<void> {
    if (!this.backend) return;
    await this.enqueue(async () => {
      if (this.mode === "offline") { this.mode = "syncing"; this.message = ""; this.emit(); }
      await this.sendAll();
    });
  }

  /** Resolves when every queued server operation has finished (tests). */
  idle(): Promise<void> {
    return this.chain;
  }

  private push(id: string) {
    if (!this.backend) return;
    void this.enqueue(async () => { if (this.mode !== "offline") await this.send(id); });
  }

  private enqueue(fn: () => Promise<void>): Promise<void> {
    this.chain = this.chain.then(fn, fn).catch(() => {});
    return this.chain;
  }

  private async sendAll() {
    for (const id of new Set([...this.deleted, ...this.dirty, ...this.links.keys(), ...this.items.filter((s) => !this.server.has(s.id)).map((s) => s.id)])) {
      if (this.mode === "offline") return;
      await this.send(id);
    }
    if (this.mode !== "offline") { this.mode = "synced"; this.emit(); }
  }

  /** Sends one selection's pending state: delete, else upsert, then links. */
  private async send(id: string) {
    const b = this.backend!;
    if (this.deleted.has(id)) {
      try {
        await b.remove(id);
      } catch (e) {
        if (statusOf(e) !== 404) {
          if (keepsPending(e)) return this.offline(e, "delete not sent");
          this.deleted.delete(id);
          return this.refused(e, "delete refused");
        }
      }
      this.deleted.delete(id);
      this.server.delete(id);
      return this.synced();
    }
    let cur = this.get(id);
    if (!cur) { this.dirty.delete(id); this.links.delete(id); return; }
    const needsUpsert = this.dirty.has(id) || !this.server.has(id);
    try {
      if (needsUpsert) {
        this.dirty.delete(id);
        let saved: Selection;
        if (this.server.has(id)) {
          saved = await b.update(id, cur).catch((e) => (statusOf(e) === 404 ? b.create(cur!) : Promise.reject(e)));
        } else {
          saved = await b.create(cur).catch((e) => (statusOf(e) === 409 ? b.update(id, cur!) : Promise.reject(e)));
        }
        this.server.set(id, saved);
        if (!this.dirty.has(id) && !this.links.has(id) && this.get(id)) this.replace(saved);
      }
      const pending = this.links.get(id);
      if (pending) {
        this.links.delete(id);
        let saved = this.server.get(id)!;
        for (const [i, l] of pending.entries()) {
          try {
            saved = await b.link(id, l);
          } catch (e) {
            this.links.set(id, [...pending.slice(i), ...(this.links.get(id) ?? [])]);
            throw e;
          }
        }
        this.server.set(id, saved);
        if (!this.dirty.has(id) && !this.links.has(id) && this.get(id)) this.replace(saved);
      }
      this.synced();
    } catch (e) {
      if (keepsPending(e)) {
        if (needsUpsert && this.get(id)) this.dirty.add(id);
        return this.offline(e, "changes kept in this page");
      }
      // Refused (400/404/409...): roll back to the server's copy, or drop what it never had.
      this.dirty.delete(id);
      this.links.delete(id);
      const known = this.server.get(id);
      cur = this.get(id);
      if (known) this.replace(known);
      else if (cur) { this.items = this.items.filter((s) => s.id !== id); this.picked.delete(id); }
      this.refused(e, `"${cur?.name ?? id}" refused by the server`);
    }
  }

  private replace(next: Selection) {
    this.items = this.items.map((s) => (s.id === next.id ? next : s));
  }

  private synced() {
    if (this.mode !== "offline") this.mode = this.sync().pending ? "syncing" : "synced";
    this.emit();
  }

  private offline(e: unknown, what: string) {
    this.mode = "offline";
    this.message = statusOf(e) === 401 ? `${what}: token rejected (${reactionTo(e).message})` : `${what}: ${errText(e)}`;
    this.emit();
  }

  private refused(e: unknown, what: string) {
    this.message = `${what}: ${errText(e)}`;
    this.emit();
  }

  private emit() {
    for (const fn of this.listeners) fn(this.items);
  }
}

/** One line for the panel: where selections live and what is pending. */
export function syncText(st: SyncState, count: number): string {
  const n = `${count} region${count === 1 ? "" : "s"}`;
  switch (st.mode) {
    case "local": return `${n} (this page only)`;
    case "synced": return `${n} · saved on the server${st.message ? ` · ${st.message}` : ""}`;
    case "syncing": return `${n} · syncing${st.pending ? ` ${st.pending} change${st.pending === 1 ? "" : "s"}` : ""}…${st.message ? ` · ${st.message}` : ""}`;
    case "offline": return `${n} · offline: kept in this page, ${st.pending} change${st.pending === 1 ? "" : "s"} will sync when the server is back · ${st.message}`;
  }
}

// ---- actions ----
//
// Hook contract (stable for T-061, which replaces `demod` and `record` with per-selection output
// chains; keep these signatures):
// - `inspect(s)`, `demod(s)`, `record(s)`: `Promise<ActionOutcome>`; a `link` in a "done" outcome
//   is stored on the selection (`POST /api/selections/<id>/links`), e.g.
//   `{ kind: "recording", target: <Recording id> }` or `{ kind: "bitstream", target: <Bitstream id> }`.
// - `listen(s)`: synchronous (it must create the AudioContext inside the click gesture); may
//   return an outcome.

export type ActionOutcome =
  | { status: "done"; message: string; link?: NewLink }
  | { status: "unavailable"; message: string }
  | { status: "failed"; message: string };

export interface SelectionActionHooks {
  inspect(s: Selection): Promise<ActionOutcome>;
  listen(s: Selection): ActionOutcome | void;
  demod(s: Selection): Promise<ActionOutcome>;
  record(s: Selection): Promise<ActionOutcome>;
}

/**
 * Runs `action` on each selection in `targets` (one, or the ticked ones), storing links of the
 * outcomes that report one. Listen starts synchronously for the first target only (one player).
 */
export async function runSelectionAction(action: ActionId, targets: readonly Selection[], store: SelectionStore, hooks: SelectionActionHooks): Promise<ActionOutcome[]> {
  if (action === "listen") {
    const s = targets[0];
    if (!s) return [];
    return [hooks.listen(s) ?? { status: "done", message: `listening to ${s.name}` }];
  }
  const out: ActionOutcome[] = [];
  for (const s of targets) {
    let r: ActionOutcome;
    try {
      r = await hooks[action](s);
    } catch (e) {
      r = { status: "failed", message: `${s.name}: ${errText(e)}` };
    }
    if (r.status === "done" && r.link) store.link(s.id, r.link);
    out.push(r);
  }
  return out;
}

interface RunState { center_hz: number; sample_rate_hz: number; recording?: { active: boolean } }

/**
 * Record hook (until T-061): the manual recorder (`POST /api/control/record/start`, T-050) records
 * the whole tuned window, so it starts only when that window covers the selection; otherwise it
 * reports that band-limited recording is coming in T-061.
 */
export async function recordSelection(client: Pick<ApiClient, "get" | "post">, s: Selection): Promise<ActionOutcome> {
  let run: RunState | null;
  try {
    run = (await client.get<{ run: RunState | null }>("/api/control/state")).run;
  } catch (e) {
    return { status: "failed", message: `record: ${reactionTo(e).message}` };
  }
  if (!run) return { status: "unavailable", message: "record: no running pipeline on this server" };
  const lo = run.center_hz - run.sample_rate_hz / 2, hi = run.center_hz + run.sample_rate_hz / 2;
  if (s.f_lo < lo || s.f_hi > hi) {
    return { status: "unavailable", message: `record "${s.name}": the tuned window ${(lo / 1e6).toFixed(3)}–${(hi / 1e6).toFixed(3)} MHz does not cover it; recording a band by itself is coming in T-061` };
  }
  const label = Array.from(`selection ${s.name}`).slice(0, 64).join("");
  try {
    const r = await client.post<{ recording: { id: string | null } }>("/api/control/record/start", { label });
    return {
      status: "done",
      message: `recording the tuned window for "${s.name}" (stop it in Controls); per-selection outputs arrive in T-061`,
      link: { kind: "recording", target: r.recording?.id ?? `manual:${label}`, note: "tuned window" },
    };
  } catch (e) {
    return { status: "failed", message: `record "${s.name}": ${reactionTo(e).message}` };
  }
}

/** Demod hook (until T-061): per-selection demodulated outputs do not exist yet. */
export async function demodSelection(s: Selection): Promise<ActionOutcome> {
  return { status: "unavailable", message: `demod "${s.name}": bitstream, symbol and WAV outputs per selection are coming in T-061; Listen streams audio now` };
}

/** A ranked explanation (hk-pipeline `family::Explanation`, as `/api/inventory` serves it). */
export interface Explanation { rank: number; service: string; label: string; score: number; flags: string[] }

/** The inventory row fields the inspection uses. */
export interface InspectRow {
  id: string; f_center_hz: number; bandwidth_hz: number; f_lo_hz: number; f_hi_hz: number;
  count: number; last_seen_s: number; known_status: string; family: string | null; explanations?: Explanation[];
}

export interface InspectReport { selection: Selection; emitters: (InspectRow & { top: Explanation[] })[]; more: boolean }

/** Emitters overlapping the selection, busiest first, each with its top-k explanations. */
export function inspectRows(s: Selection, rows: readonly InspectRow[], k = 3): InspectReport["emitters"] {
  return rows
    .filter((r) => r.f_hi_hz >= s.f_lo && r.f_lo_hz <= s.f_hi)
    .sort((a, b) => b.count - a.count || b.last_seen_s - a.last_seen_s)
    .map((r) => ({ ...r, top: [...(r.explanations ?? [])].sort((a, b) => a.rank - b.rank).slice(0, k) }));
}

/** The inventory query for a selection (frequency extent, plus its time extent when set). */
export function inspectQuery(s: Selection, limit = 50): string {
  const q = new URLSearchParams({ f_lo: String(s.f_lo), f_hi: String(s.f_hi) });
  if (s.t_lo !== undefined && s.t_hi !== undefined) { q.set("t0", String(s.t_lo)); q.set("t1", String(s.t_hi)); }
  q.set("limit", String(limit));
  return `/api/inventory?${q}`;
}

/** Inspect hook: fetches the emitters inside and reports them; links the busiest one. */
export async function inspectSelection(get: (path: string) => Promise<unknown>, s: Selection, k = 3): Promise<{ outcome: ActionOutcome; report: InspectReport | null }> {
  try {
    const page = (await get(inspectQuery(s))) as { entries: InspectRow[]; next_cursor?: string | null };
    const emitters = inspectRows(s, page.entries, k);
    const report = { selection: s, emitters, more: !!page.next_cursor };
    const top = emitters[0];
    return {
      report,
      outcome: {
        status: "done",
        message: `${s.name}: ${emitters.length}${report.more ? "+" : ""} emitter${emitters.length === 1 ? "" : "s"} inside`,
        link: { kind: "inspection", target: top ? top.id : "no-emitters", note: `${emitters.length} emitters` },
      },
    };
  } catch (e) {
    return { report: null, outcome: { status: "failed", message: `inspect "${s.name}": ${errText(e)}` } };
  }
}
