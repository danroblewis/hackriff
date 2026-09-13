// Client-side region selections (T-044). Several can exist at once; each is a first-class object
// with an id, a name and a frequency extent, optionally a time extent. Pure (no DOM): unit-tested
// in ui/test/selections.test.ts. T-052 persists them server-side and wires the actions; until
// then the actions below are stubs and nothing leaves the page.

/** One selected region. Frequencies in Hz; times in Unix seconds (floats), like the API. */
export interface Selection {
  id: string;
  name: string;
  f_lo: number;
  f_hi: number;
  t_lo?: number;
  t_hi?: number;
  /** When it was made, Unix seconds. */
  created: number;
}

/** What a drag (or a caller) supplies; id, created and a default name are filled in. */
export interface NewSelection {
  name?: string;
  f_lo: number;
  f_hi: number;
  t_lo?: number;
  t_hi?: number;
}

/** Actions a selection will offer. All stubs until T-052 (they are rendered disabled). */
export const SELECTION_ACTIONS = [
  { id: "inspect", label: "Inspect", task: "T-052" },
  { id: "demod", label: "Demod", task: "T-052" },
  { id: "record", label: "Record", task: "T-052" },
] as const;

export const MAX_NAME_LEN = 64;

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

const cleanName = (name: string) => name.replace(/\s+/g, " ").trim().slice(0, MAX_NAME_LEN);

export interface StoreOptions {
  /** Clock, Unix seconds (tests inject one). */
  now?: () => number;
  /** Id generator (tests inject one). */
  newId?: () => string;
}

function randomId(): string {
  const b = new Uint8Array(8);
  crypto.getRandomValues(b);
  return Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
}

/** The selections of this page, in creation order. */
export class SelectionStore {
  private items: Selection[] = [];
  private listeners = new Set<(list: readonly Selection[]) => void>();
  private counter = 0;
  private now: () => number;
  private newId: () => string;

  constructor(opts: StoreOptions = {}) {
    this.now = opts.now ?? (() => Date.now() / 1000);
    this.newId = opts.newId ?? randomId;
  }

  list(): readonly Selection[] {
    return this.items;
  }

  get(id: string): Selection | undefined {
    return this.items.find((s) => s.id === id);
  }

  /** Adds a selection; throws on an invalid one (see [`validateSelection`]). */
  add(s: NewSelection): Selection {
    const problem = validateSelection(s);
    if (problem) throw new Error(problem);
    this.counter++;
    const sel: Selection = {
      id: this.newId(),
      name: s.name !== undefined ? cleanName(s.name) : `Region ${this.counter}`,
      f_lo: s.f_lo,
      f_hi: s.f_hi,
      created: this.now(),
    };
    if (s.t_lo !== undefined) { sel.t_lo = s.t_lo; sel.t_hi = s.t_hi; }
    this.items = [...this.items, sel];
    this.emit();
    return sel;
  }

  /** Renames; false when the id is unknown or the name is empty after trimming. */
  rename(id: string, name: string): boolean {
    const clean = cleanName(name);
    const i = this.items.findIndex((s) => s.id === id);
    if (i < 0 || clean === "") return false;
    this.items = this.items.map((s, j) => (j === i ? { ...s, name: clean } : s));
    this.emit();
    return true;
  }

  remove(id: string): boolean {
    const before = this.items.length;
    this.items = this.items.filter((s) => s.id !== id);
    if (this.items.length === before) return false;
    this.emit();
    return true;
  }

  clear() {
    if (!this.items.length) return;
    this.items = [];
    this.emit();
  }

  /** Calls `fn` after every change; returns an unsubscribe function. */
  subscribe(fn: (list: readonly Selection[]) => void): () => void {
    this.listeners.add(fn);
    return () => this.listeners.delete(fn);
  }

  private emit() {
    for (const fn of this.listeners) fn(this.items);
  }
}
