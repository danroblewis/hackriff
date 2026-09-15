// MUI state store (ADR-0013 §3): one immutable state object, shallow top-level patches, and
// selector subscriptions that fire only when the selected value changes. No dependencies; small on
// purpose, so every panel and test uses the same contract.
//
// Rules:
// - State is replaced, never mutated: `set` merges a top-level patch into a new object, and a
//   slice that changes must be a new object/array (panels compare selected values with Object.is).
// - `set` called from inside a listener is queued and applied after the current notification
//   pass, so listeners always see a consistent (state, prev) pair.
// - One panel's bug can't starve the others: a listener that throws is logged (with its function
//   name when it has one) and the remaining listeners still run; queued patches still flush.

export type Patch<S> = Partial<S> | ((s: S) => Partial<S>);
export type Equals<T> = (a: T, b: T) => boolean;

export interface Store<S> {
  get(): S;
  set(patch: Patch<S>): void;
  /** Called after every applied patch. Returns an unsubscribe function. */
  subscribe(fn: (s: S, prev: S) => void): () => void;
  /** Calls `fn(value, prev)` whenever `sel(state)` changes (`eq`, default Object.is); with
   * `immediate`, also once now. Returns an unsubscribe function. */
  select<T>(sel: (s: S) => T, fn: (v: T, prev: T | undefined) => void, opts?: { eq?: Equals<T>; immediate?: boolean }): () => void;
}

interface Listener<S> { call: (s: S, prev: S) => void; name: string }

export function createStore<S extends object>(initial: S): Store<S> {
  let state = initial;
  const listeners = new Set<Listener<S>>();
  const queue: Patch<S>[] = [];
  let notifying = false;

  const add = (l: Listener<S>) => {
    listeners.add(l);
    return () => { listeners.delete(l); };
  };

  const apply = (p: Patch<S>) => {
    const part = typeof p === "function" ? p(state) : p;
    const keys = Object.keys(part) as (keyof S)[];
    if (!keys.some((k) => !Object.is(part[k], state[k]))) return;
    const prev = state;
    state = { ...state, ...part };
    notifying = true;
    try {
      for (const l of [...listeners]) {
        try {
          l.call(state, prev);
        } catch (err) {
          console.error(`store listener ${l.name} threw`, err);
        }
      }
    } finally {
      notifying = false;
    }
  };

  return {
    get: () => state,
    set(p) {
      queue.push(p);
      if (notifying) return;
      // A throwing patch function is reported to the caller, but only after the rest of the
      // queue (patches queued by listeners) has been applied.
      let failed: { err: unknown } | null = null;
      while (queue.length) {
        try {
          apply(queue.shift()!);
        } catch (err) {
          failed ??= { err };
        }
      }
      if (failed) throw failed.err;
    },
    subscribe(fn) {
      return add({ call: fn, name: fn.name || "(anonymous subscribe)" });
    },
    select(sel, fn, opts = {}) {
      const eq = opts.eq ?? Object.is;
      let last = sel(state);
      if (opts.immediate) fn(last, undefined);
      return add({
        name: fn.name || "(anonymous select)",
        call: (s) => {
          const v = sel(s);
          if (eq(v, last)) return;
          const prev = last;
          last = v;
          fn(v, prev);
        },
      });
    },
  };
}
