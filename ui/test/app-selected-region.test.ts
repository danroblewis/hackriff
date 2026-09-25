// T-943: the SELECTED-REGION panel — what it lists, and what it lets you do.
//
// Two faults the explorer found on the staging build (2026-09-25, live HackRF in SF), both in the
// one sheet titled "Selected region":
//
//   (a) it listed signals from OUTSIDE the region — a region at 98.8226–99.0407 MHz headed by
//       107.816 / 106.997 / 106.159 / 107.662 MHz as "Unknown & unexplained", and 106.166 MHz as
//       "Strongest" (shots/0404-region-989.png). The whole UI is scoped to one (time × frequency)
//       window (CLAUDE.md), the region panel included. The drawer's half of that is guarded in
//       `app-explore-drawer.test.ts` at the REQUEST level; this file guards the panel itself.
//   (b) it offered no action at all — no Listen, no Decode, just a sentence telling the viewer to
//       right-click — and a right-click on a list row opened nothing. An action that exists only in
//       a context menu that does not open is an action that does not exist.
//
// No DOM under node:test, so the panel is rendered into a minimal fake DOM (the pattern of
// app-sheet-principles.test.ts) that is complete enough for the real `contextMenu()` component to
// open into, because "the menu opens" is precisely the claim.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { renderSelectionFocus } from "../src/app/explore";
import { setInventoryRows } from "../src/app/explore/slice";
import type { Row } from "../src/app/explore/inventory";
import type { Selection } from "../src/app/explore/selections";
import { SELECTION_DETAIL_ACTIONS, detailActions } from "../src/app/explore/detail";
import { selectionMenuItems } from "../src/app/menu/actions";
import { contextMenu } from "../src/app/menu/menu";

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: (FakeEl | string)[] = [];
  parent: FakeEl | null = null;
  attrs: Record<string, string> = {};
  style: Record<string, string> = {};
  className = ""; textContent = ""; hidden = false; type = "";
  focused = false;
  handlers: Record<string, Handler[]> = {};
  classList = { add: () => {}, remove: () => {}, contains: () => false };
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) { if (typeof x !== "string") x.parent = this; this.children.push(x); } }
  replaceChildren(...c: (FakeEl | string)[]) { this.children = []; this.append(...c); }
  setAttribute(k: string, v: string) { this.attrs[k] = v; if (k === "class") this.className = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  removeAttribute(k: string) { delete this.attrs[k]; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  removeEventListener() {}
  focus() { this.focused = true; }
  contains(el: unknown) { return walk(this).includes(el as FakeEl); }
  getBoundingClientRect() { return { x: 0, y: 0, width: 180, height: 220, bottom: 220, right: 180 }; }
  get classes(): string[] { return (this.attrs.class ?? this.className).split(/\s+/).filter(Boolean); }
  get text(): string { return this.textContent + this.children.map((c) => (typeof c === "string" ? c : c.text)).join(""); }
  matches(sel: string): boolean {
    const parts = sel.match(/\.[-\w]+|\[[-\w]+\]/g) ?? [];
    return parts.length > 0 && parts.every((p) => p.startsWith(".") ? this.classes.includes(p.slice(1)) : this.attrs[p.slice(1, -1)] !== undefined);
  }
  closest(sel: string): FakeEl | null {
    for (let e: FakeEl | null = this; e; e = e.parent) if (e.matches(sel)) return e;
    return null;
  }
  fire(t: string, ev: Record<string, unknown> = {}) {
    for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, stopPropagation() {}, button: 0, touches: [], target: this, currentTarget: this, ...ev });
  }
}

function walk(el: FakeEl, out: FakeEl[] = []): FakeEl[] {
  out.push(el);
  for (const c of el.children) if (typeof c !== "string") walk(c, out);
  return out;
}

const body = new FakeEl("body");
const g = globalThis as Record<string, unknown>;
g.document = {
  createElement: (t: string) => new FakeEl(t),
  querySelector: () => null,
  body,
  activeElement: null,
  addEventListener() {}, removeEventListener() {},
  contains: (el: unknown) => body.contains(el),
};
// `menu.ts` asks `document.activeElement instanceof HTMLElement` before remembering the opener.
g.HTMLElement = FakeEl;
g.window = { innerWidth: 1200, innerHeight: 1000, addEventListener() {}, setTimeout: () => 0, clearTimeout: () => {} };

const T0 = 1_789_300_000;
/** The explorer's own region: 98.8226–99.0407 MHz (journal-20260925.md). */
const REGION: Selection = { id: "s1", name: "Region 1", f_lo: 98_822_600, f_hi: 99_040_700, tags: [], links: [], created: T0, updated: T0 };

function row(id: string, centerHz: number, over: Partial<Row> = {}): Row {
  const bw = 180_000;
  return {
    id, state: "candidate", f_center_hz: centerHz, bandwidth_hz: bw,
    f_lo_hz: centerHz - bw / 2, f_hi_hz: centerHz + bw / 2,
    first_seen_s: T0 - 60, last_seen_s: T0, count: 7,
    known_status: "unknown", status: null, tags: [], family: null,
    identity_scheme: null, identity_class: null, withheld: false,
    recurrence: null, classification: null, explanations: [], refined: null,
    cluster_id: null, cluster_group: null,
    ...over,
  } as unknown as Row;
}

function ctxWith(rows: Row[]): { ctx: AppContext; posts: string[] } {
  const posts: string[] = [];
  const client = {
    get: async () => { throw new Error("unexpected GET"); },
    post: async (path: string, b?: unknown) => { posts.push(`POST ${path} ${JSON.stringify(b)}`); return {} as never; },
    put: async () => { throw new Error("unexpected PUT"); },
    del: async () => { throw new Error("unexpected DELETE"); },
  } as unknown as AppContext["client"];
  const store = createStore(initialState());
  store.set(setInventoryRows(Object.fromEntries(rows.map((r) => [r.id, r])), T0));
  store.set(() => ({ selections: { list: [REGION], sync: "" }, focus: { kind: "selection", id: REGION.id } }));
  return { ctx: { store, client, token: "t" } as unknown as AppContext, posts };
}

const panel = (ctx: AppContext) => renderSelectionFocus(ctx, REGION) as unknown as FakeEl;
const actionsOf = (root: FakeEl) => walk(root).filter((e) => e.attrs["data-action"]);

test("T-943 (b): the region panel offers Listen and Decode as visible, labelled buttons", () => {
  const { ctx } = ctxWith([row("in1", 98_900_000)]);
  const root = panel(ctx);
  const cluster = walk(root).find((e) => e.attrs.role === "toolbar" && e.classes.includes("actions"));
  assert.ok(cluster, "the region panel has an action cluster (it had none: only a right-click hint)");
  const ids = actionsOf(root).map((e) => e.attrs["data-action"]);
  assert.deepEqual(ids, ["listen-all", "decode", "export", "analyze", "delete"]);
  const labels = actionsOf(root).map((e) => e.text);
  assert.ok(labels.some((l) => /Listen/.test(l)), `a Listen button, got ${JSON.stringify(labels)}`);
  assert.ok(labels.some((l) => /Decode/.test(l)), `a Decode button, got ${JSON.stringify(labels)}`);
  for (const b of actionsOf(root)) {
    assert.equal(b.tag, "button");
    assert.equal(b.attrs.type, "button");
    assert.ok(b.attrs["aria-label"], "labelled for the keyboard / a screen reader");
  }
});

test("T-943 (b): Listen on a region starts one stream per signal inside it; Decode opens the workbench", () => {
  const { ctx } = ctxWith([row("in1", 98_900_000), row("far", 107_816_000)]);
  const root = panel(ctx);
  const byId = (id: string) => actionsOf(root).find((e) => e.attrs["data-action"] === id)!;
  // Decode is a mode change (view state), never a device route — the same call the menu item makes.
  byId("decode").fire("click");
  assert.equal(ctx.store.get().mode, "decode");
  // Listen's targets are the rows INSIDE the region, never every loaded row: `listenAllTargets`
  // over `foundInside`. Asserted on the built items rather than by starting real audio (an
  // AudioContext needs a browser — app-dock.test.ts's note).
  const items = selectionMenuItems(ctx, REGION, [row("in1", 98_900_000)]);
  const listen = items.find((i) => i.id === "listen-all")!;
  assert.equal(listen.hint, "1 stream at once");
  assert.equal(listen.disabled, false);
  // Nothing inside: the button is there, and honestly disabled rather than absent.
  const empty = detailActions(selectionMenuItems(ctx, REGION, []), SELECTION_DETAIL_ACTIONS);
  assert.equal(empty.find((a) => a.id === "listen-all")!.disabled, true);
  assert.deepEqual(empty.map((a) => a.id), ["listen-all", "decode", "export", "analyze", "delete"]);
});

test("T-943 (a): the region panel's own list holds only signals inside the region", () => {
  // The explorer's shot: a 98.8226–99.0407 MHz region listing 107.816 / 106.997 / 106.159 MHz.
  const { ctx } = ctxWith([row("in1", 98_900_000), row("f1", 107_816_000), row("f2", 106_997_000), row("f3", 106_159_000)]);
  const root = panel(ctx);
  const listed = walk(root).filter((e) => e.classes.includes("row") && e.attrs["data-id"]);
  assert.deepEqual(listed.map((e) => e.attrs["data-id"]), ["in1"]);
});

test("T-943 (b): a right-click on a region row opens the signal menu, with Listen in it", () => {
  const { ctx } = ctxWith([row("in1", 98_900_000)]);
  const root = panel(ctx);
  const rowEl = walk(root).find((e) => e.classes.includes("row") && e.attrs["data-id"] === "in1")!;
  // The trigger is bound on the list (one listener set, as `bindContextTrigger` documents), so a
  // real right-click on a row arrives there with the row as its target — that is what is fired.
  const list = rowEl.parent!;
  assert.equal(contextMenu().isOpen(), false);
  list.fire("contextmenu", { clientX: 120, clientY: 300, target: rowEl });
  assert.equal(contextMenu().isOpen(), true, "right-clicking a row in the region panel opened nothing");
  const menu = walk(body).find((e) => e.classes.includes("ctx-menu"))!;
  const labels = menu.children.filter((c): c is FakeEl => typeof c !== "string").map((c) => c.text);
  assert.ok(labels.some((l) => /^Listen/.test(l)), `Listen among ${JSON.stringify(labels)}`);
  assert.ok(labels.some((l) => /Decode/.test(l)), `Decode among ${JSON.stringify(labels)}`);
  contextMenu().close();
});
