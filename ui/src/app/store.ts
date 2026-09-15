// MUI state store (ADR-0013 §3): one immutable state object, shallow top-level patches, and
// selector subscriptions that fire only when the selected value changes. No dependencies; ~60
// lines on purpose, so every panel and test uses the same small contract.
//
// Rules:
// - State is replaced, never mutated: `set` merges a top-level patch into a new object, and a
//   slice that changes must be a new object/array (panels compare selected values with Object.is).
// - `set` called from inside a listener is queued and applied after the current notification
//   pass, so listeners always see a consistent (state, prev) pair.

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

export function createStore<S extends object>(initial: S): Store<S> {
  let state = initial;
  const listeners = new Set<(s: S, prev: S) => void>();
  const queue: Patch<S>[] = [];
  let notifying = false;

  const apply = (p: Patch<S>) => {
    const part = typeof p === "function" ? p(state) : p;
    const keys = Object.keys(part) as (keyof S)[];
    if (!keys.some((k) => !Object.is(part[k], state[k]))) return;
    const prev = state;
    state = { ...state, ...part };
    notifying = true;
    try {
      for (const fn of [...listeners]) fn(state, prev);
    } finally {
      notifying = false;
    }
  };

  return {
    get: () => state,
    set(p) {
      queue.push(p);
      if (notifying) return;
      while (queue.length) apply(queue.shift()!);
    },
    subscribe(fn) {
      listeners.add(fn);
      return () => listeners.delete(fn);
    },
    select(sel, fn, opts = {}) {
      const eq = opts.eq ?? Object.is;
      let last = sel(state);
      if (opts.immediate) fn(last, undefined);
      const l = (s: S) => {
        const v = sel(s);
        if (eq(v, last)) return;
        const prev = last;
        last = v;
        fn(v, prev);
      };
      listeners.add(l);
      return () => listeners.delete(l);
    },
  };
}
