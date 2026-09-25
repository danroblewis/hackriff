// docs/23 §10.6 (the user's map-UI principles, 2026-09-24) as they bind the Selected sheet (T-804 on
// the T-803 bottom sheet):
//   P4 — size inversely proportional to influence. The sheet is a big panel, so its BODY only shows
//        (and at most changes marks, e.g. which signal is focused). Anything that reaches the device
//        (the gated DeviceAction path, any docs/23 §11 write) or moves the map in a major way
//        (retune, zoom, jump, follow live, switching mode) lives only on SMALL labelled buttons in a
//        compact cluster: >= 24 px hit targets, keyboard reachable.
//   P1 — a visible dismiss that gives the sheet's pixels back to the map.
// No DOM under node:test, so the sheet's content is rendered into a minimal fake DOM and walked:
// every element that carries a handler is either a small button inside the action cluster, or its
// handlers are fired against a spy client and a store snapshot and must reach no route and move no
// view state. A body element bound to a device route or a view jump turns this red.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { renderSelectionFocus, renderSignalFocus } from "../src/app/explore";
import type { Row } from "../src/app/explore/inventory";
import type { Selection } from "../src/app/explore/selections";
import { mountSheet } from "../src/app/chrome/sheet";

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: (FakeEl | string)[] = [];
  parent: FakeEl | null = null;
  attrs: Record<string, string> = {};
  dataset: Record<string, string> = {};
  style: Record<string, string> = {};
  className = "";
  textContent = "";
  hidden = false;
  inert = false;
  type = "";
  handlers: Record<string, Handler[]> = {};
  classList = { add: () => {}, remove: () => {}, contains: () => false };
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) { if (typeof x !== "string") x.parent = this; this.children.push(x); } }
  prepend(...c: FakeEl[]) { for (const x of c) x.parent = this; this.children.unshift(...c); }
  replaceChildren(...c: FakeEl[]) { this.children = []; this.append(...c); }
  setAttribute(k: string, v: string) { this.attrs[k] = v; if (k === "class") this.className = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  querySelector(sel: string) { return sel === ":scope > .sheet-body" ? this.children.find((c) => typeof c !== "string" && c.className === "sheet-body") ?? null : null; }
  /** Enough of `matches`/`closest` for the selectors the panels resolve a menu target with
   * (`.row[data-id]`, `[data-emitter]`): classes and attribute presence, all of which must hold. */
  matches(sel: string): boolean {
    const parts = sel.match(/\.[-\w]+|\[[-\w]+\]/g) ?? [];
    return parts.length > 0 && parts.every((p) => p.startsWith(".") ? this.classes.includes(p.slice(1)) : this.attrs[p.slice(1, -1)] !== undefined);
  }
  closest(sel: string): FakeEl | null {
    for (let e: FakeEl | null = this; e; e = e.parent) if (e.matches(sel)) return e;
    return null;
  }
  getBoundingClientRect() { return { height: parseFloat(this.style.height ?? "56"), bottom: 930 }; }
  setPointerCapture() {}
  get text(): string { return this.textContent + this.children.map((c) => (typeof c === "string" ? c : c.text)).join(""); }
  get classes(): string[] { return (this.attrs.class ?? this.className).split(/\s+/).filter(Boolean); }
  fire(t: string, ev: Record<string, unknown> = {}) {
    // `touches: []` by default: the audit fires EVERY handler an element carries, and a
    // long-press trigger (`menu/trigger.ts`) reads `e.touches` on touchstart. An empty list is the
    // cancel path — no timer, no menu — which is what a synthetic fire should be.
    for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, stopPropagation() { ev.stopped = true; }, button: 0, pointerId: 1, touches: [], target: this, currentTarget: this, ...ev });
  }
}

function withFakeDom<T>(fn: () => T): T {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = { createElement: (t: string) => new FakeEl(t), querySelector: () => null };
  g.window = { innerHeight: 1000, addEventListener() {}, setTimeout: () => 0, clearTimeout: () => {} };
  try { return fn(); } finally { g.document = saved.document; g.window = saved.window; }
}

function walk(el: FakeEl, out: FakeEl[] = []): FakeEl[] {
  out.push(el);
  for (const c of el.children) if (typeof c !== "string") walk(c, out);
  return out;
}

const inCluster = (el: FakeEl): boolean => {
  for (let p = el.parent; p; p = p.parent) if (p.attrs.role === "toolbar" && p.classes.includes("actions")) return true;
  return false;
};

function spyCtx() {
  const calls: string[] = [];
  const client = new Proxy({}, { get: (_t, k) => (...a: unknown[]) => { calls.push(`${String(k)} ${JSON.stringify(a)}`); return Promise.resolve({}); } });
  const origFetch = globalThis.fetch;
  globalThis.fetch = ((...a: unknown[]) => { calls.push(`fetch ${JSON.stringify(a[0])}`); return Promise.reject(new Error("spy")); }) as typeof fetch;
  const ctx = { store: createStore(initialState()), client, token: "t" } as unknown as AppContext;
  return { ctx, calls, restore: () => { globalThis.fetch = origFetch; } };
}

const T0 = Date.UTC(2026, 8, 13, 12, 0, 0) / 1000;
function row(over: Partial<Row> = {}): Row {
  return {
    id: "e1", state: "candidate",
    f_center_hz: 100_800_000, bandwidth_hz: 181_400, f_lo_hz: 100_709_300, f_hi_hz: 100_890_700,
    first_seen_s: T0 - 600, last_seen_s: T0, count: 42,
    known_status: "known", status: null, tags: [], family: "wfm",
    identity_scheme: "rds-pi", identity_value: "C201", identity_class: null, withheld: false,
    recurrence: null, classification: null, refined: null,
    explanations: [{ rank: 1, label: "FM broadcast", score: 0.8, flags: ["off-raster"], evidence: [] }],
    cluster_id: "c1", cluster_group: null, user_band: null, relation: null,
    presence: {
      intervals: 1, on_air_s: 30, liveness: "live", ended_t_s: null,
      last_interval: { t_start_s: T0 - 30, t_end_s: T0 - 1, open: true, revoked_s: 0 },
    },
    measured: { snr_db: 21.4, peak_dbfs: -18.25, t_start_s: T0 - 3, t_end_s: T0 - 2, duration_s: 1 },
    ...over,
  } as unknown as Row;
}
const SEL = { id: "s1", name: "Region 1", f_lo: 100_700_000, f_hi: 100_900_000, tags: [], links: [], created: T0, updated: T0 } as Selection;

/** The store with the parts that are only MARKS (which signal is focused, which list tab shows it)
 * removed: everything left is view or device state a sheet body must never move. */
function viewState(ctx: AppContext): string {
  const { focus: _f, inventory, ...rest } = ctx.store.get() as unknown as Record<string, unknown> & { inventory: Record<string, unknown> };
  const { tab: _t, ...inv } = inventory;
  return JSON.stringify({ ...rest, inventory: inv });
}

/** Renders one sheet body and checks every handler-carrying element against P4. */
function auditBody(name: string, render: (ctx: AppContext) => FakeEl) {
  const spy = spyCtx();
  try {
    withFakeDom(() => {
      spy.ctx.store.set((s) => ({ inventory: { ...s.inventory, rows: { e1: row() } } }));
      const root = render(spy.ctx);
      const els = walk(root);
      const cluster = els.filter((e) => e.attrs.role === "toolbar" && e.classes.includes("actions"));
      const bound = els.filter((e) => Object.keys(e.handlers).length > 0);
      for (const el of bound.filter(inCluster)) {
        assert.equal(el.tag, "button", `${name}: an action in the cluster is a button (${el.text})`);
        assert.equal(el.attrs.type, "button");
        assert.notEqual(el.attrs.tabindex, "-1", "keyboard reachable");
        assert.ok(el.text.trim() && el.attrs["aria-label"], `${name}: a labelled button, not an icon alone`);
      }
      // Everything else that listens is body content: it may change marks, nothing more.
      for (const el of bound.filter((e) => !inCluster(e))) {
        const before = viewState(spy.ctx);
        for (const t of Object.keys(el.handlers)) el.fire(t, { key: "Enter", clientX: 10, clientY: 10 });
        assert.deepEqual(spy.calls, [], `${name}: a body <${el.tag} class="${el.classes.join(" ")}"> reached a route`);
        assert.equal(viewState(spy.ctx), before, `${name}: a body <${el.tag} class="${el.classes.join(" ")}"> moved the view`);
      }
      return cluster;
    });
  } finally {
    spy.restore();
  }
}

test("P4: the signal detail sheet's device actions are only on the small-button cluster", () => {
  auditBody("signal", (ctx) => renderSignalFocus(ctx, row(), null, null) as unknown as FakeEl);
  const spy = spyCtx();
  try {
    withFakeDom(() => {
      const root = renderSignalFocus(spy.ctx, row(), null, null) as unknown as FakeEl;
      const els = walk(root);
      const clusters = els.filter((e) => e.attrs.role === "toolbar" && e.classes.includes("actions"));
      assert.equal(clusters.length, 1, "one action cluster");
      assert.ok(clusters[0].attrs["aria-label"], "the cluster is labelled");
      const ids = walk(clusters[0]).map((e) => e.attrs["data-action"]).filter(Boolean);
      assert.deepEqual(ids, ["listen", "decode", "export", "stream", "analyze", "promote", "delete"]);
      // Every action id lives in the cluster and nowhere else in the body.
      const outside = els.filter((e) => e.attrs["data-action"] && !inCluster(e));
      assert.deepEqual(outside.map((e) => e.attrs["data-action"]), []);
    });
  } finally {
    spy.restore();
  }
});

test("P4: the selection sheet's body reaches no route and moves no view", () => {
  auditBody("selection", (ctx) => renderSelectionFocus(ctx, SEL) as unknown as FakeEl);
});

test("P4: the action buttons are small (>= 24 px hit target, sized to the label, never a large surface)", () => {
  const css = readFileSync("src/app/explore/explore.css", "utf8");
  const rule = (sel: string) => {
    const esc = sel.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const m = css.match(new RegExp(`^${esc}\\s*\\{([^}]*)\\}`, "m"));
    assert.ok(m, `rule ${sel}`);
    return Object.fromEntries(m![1].split(";").map((d) => d.split(":").map((x) => x.trim())).filter((d) => d[0]));
  };
  const btn = rule(".focus .detail .actions button");
  assert.equal(btn["min-height"], "24px");
  assert.equal(btn["min-width"], "24px");
  assert.ok(parseFloat(btn["font-size"]) <= 12, `small text (${btn["font-size"]})`);
  const [padV] = (btn.padding ?? "0").split(/\s+/).map(parseFloat);
  assert.ok(padV <= 4, `compact padding (${btn.padding})`);
  assert.ok(!btn.width && !btn.height && btn.flex === "none", "sized to its label, never stretched");
  const row = rule(".focus .detail .actions");
  assert.notEqual(row["flex-direction"], "column", "a cluster, not a stack of full-width bars");
  assert.ok(!row.width && !row.height, "the cluster takes no fixed area of the sheet");
  // Any other rule touching the buttons (`.primary`, `.danger`, `:focus-visible`) must not enlarge them.
  for (const m of css.matchAll(/^\.focus \.detail \.actions button[.:][^{]*\{([^}]*)\}/gm)) {
    assert.doesNotMatch(m[1], /(^|;)\s*(width|height|font-size)\s*:/, `no enlarging rule: ${m[0]}`);
  }
});

test("P1: the sheet has a visible dismiss that collapses it and returns its pixels to the map", () => {
  withFakeDom(() => {
    const host = new FakeEl("section");
    const body = new FakeEl("div");
    body.className = "sheet-body";
    host.append(body);
    const ctl = mountSheet(host as unknown as HTMLElement, { storageKey: "s", label: "Selected", reservedPx: 170, storage: null });
    const head = host.children[1] as FakeEl;
    const close = head.children.find((c) => typeof c !== "string" && c.className === "sheet-close") as FakeEl | undefined;
    assert.ok(close, "a dismiss button in the sheet's head");
    assert.equal(close.tag, "button");
    assert.ok(close.attrs["aria-label"] && close.textContent, "visible and labelled");
    assert.equal(close.hidden, true, "nothing to dismiss while collapsed");
    ctl.set("half");
    assert.equal(close.hidden, false, "visible while open");
    const tall = parseFloat(host.style.height);
    const ev: Record<string, unknown> = {};
    close.fire("click", ev);
    assert.equal(ctl.get(), "peek");
    assert.ok(parseFloat(host.style.height) < tall, "the sheet's pixels go back to the map");
    assert.equal(ev.stopped, true, "does not bubble into the head's open-on-click");
    const css = readFileSync("src/app/chrome/sheet.css", "utf8");
    const rule = css.match(/^\.sheet \.sheet-close \{([^}]*)\}/m)?.[1] ?? "";
    for (const k of ["min-width", "min-height"]) {
      const px = parseFloat(rule.match(new RegExp(`${k}:\\s*([\\d.]+)px`))?.[1] ?? "0");
      assert.ok(px >= 24, `dismiss ${k} >= 24 px (${px})`);
    }
  });
});
