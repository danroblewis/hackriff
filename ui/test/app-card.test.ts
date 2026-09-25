// T-1026 (user 2026-09-25 via the supervisor): the detail card is a Google-Maps PLACE CARD — hidden
// until a feature is clicked, swapped when another is clicked, gone on a click on the back of the map
// or on Escape, and opened on a list by an inventory pill.
//
//   "The bottom right accordion panel is kind of dumb to show all the time, it should be hidden all
//    the time until someone clicks on something, like on Google Maps how someone clicks something and
//    it opens the detailed view, and if they click on the back of the map it goes away, or if they
//    click on another interest point it changes."
//
// What is tested where. The STATE MACHINE is pure (`explore/slice.ts`'s `card` actions) and is tested
// as such; the MOUNT (`chrome/focus-sheet.ts` over `chrome/sheet.ts`) is tested over a minimal fake
// DOM with a spy client, so every open, swap and dismiss is proved to reach no route; the two
// gestures that need a real canvas and a real browser hit test — clicking a box, and clicking bare
// map — are `ui/e2e/app-card.e2e.mjs`'s, at 1280x800 and at 400 px.
//
// The one source-level assertion here is the bare-map branch of the surface's click handler: node
// has no WebGL2, so the behaviour cannot be exercised, but "which branch closes the card" can be read.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { keyMove, mountSheet, PEEK_PX, releaseMove, snapHeights, FLICK_PX_PER_MS, type SheetSnap } from "../src/app/chrome/sheet";
import { overlays, wireEscape } from "../src/app/chrome/dismiss";
import { mountFocusSheet } from "../src/app/chrome/focus-sheet";
import { mountInvPills } from "../src/app/chrome/inv-pills";
import { registerMapInvHome, resetMapInvHome } from "../src/app/chrome/inv-home";
import { closeCard, focusSelection, focusSignal, openCardOnList, setInventoryRows } from "../src/app/explore/slice";
import type { AppContext } from "../src/app/context";
import type { Row } from "../src/app/explore/inventory";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";

const H = snapHeights(1000, 170);
const row = (id: string, state: string) =>
  ({ id, state, f_center_hz: 100.8e6, bandwidth_hz: 200e3, explanations: [], count: 1 }) as unknown as Row;

// ---- the state machine (pure) ----

test("the card starts hidden, and only a click puts it on screen", () => {
  const s0 = initialState();
  assert.deepEqual(s0.card, { open: false }, "the first paint of the map has no card on it");
  assert.deepEqual(s0.focus, { kind: "none" });
});

test("selecting a feature opens the card; another feature swaps it without closing it", () => {
  const store = createStore(initialState());
  store.set(setInventoryRows({ e1: row("e1", "candidate"), e2: row("e2", "confirmed") }, 1));
  store.set(focusSignal("e1"));
  assert.equal(store.get().card.open, true, "a box/pin/row click opened the card");
  assert.deepEqual(store.get().focus, { kind: "signal", id: "e1" });
  assert.equal(store.get().inventory.tab, "candidate", "and the list showing that row is the one shown");
  // Another feature: the card stays open and its subject changes — never closed-then-reopened.
  store.set(focusSignal("e2"));
  assert.equal(store.get().card.open, true);
  assert.deepEqual(store.get().focus, { kind: "signal", id: "e2" });
  store.set(focusSelection("s1"));
  assert.equal(store.get().card.open, true);
  assert.deepEqual(store.get().focus, { kind: "selection", id: "s1" });
});

test("closing the card clears the selection, so clicking the same feature re-opens it", () => {
  const store = createStore(initialState());
  store.set(focusSignal("e1"));
  store.set(closeCard());
  assert.deepEqual(store.get().card, { open: false });
  assert.deepEqual(store.get().focus, { kind: "none" },
    "a card closed over a still-selected box leaves a highlighted feature no click can re-open");
  // Idempotent: a click on bare map with nothing selected is not a state change at all.
  const before = store.get();
  store.set(closeCard());
  assert.equal(store.get(), before, "closing an already-closed card patches nothing");
  store.set(focusSignal("e1"));
  assert.equal(store.get().card.open, true, "the same feature opens it again");
});

test("a pill opens the card ON ITS LIST, and invents no selection", () => {
  const store = createStore(initialState());
  store.set(openCardOnList("candidate"));
  assert.deepEqual(store.get().card, { open: true });
  assert.equal(store.get().inventory.tab, "candidate");
  assert.deepEqual(store.get().focus, { kind: "none" }, "the list is not a selection");
  // Pressing it again keeps the same inventory object (no needless re-render of the lists).
  const inv = store.get().inventory;
  store.set(openCardOnList("candidate"));
  assert.equal(store.get().inventory, inv);
});

// ---- the dismiss gestures, as pure moves ----

test("shrinking past the strip, or flicking it down, is a DISMISS and not a smaller sheet", () => {
  const cases: [string, SheetSnap, ReturnType<typeof keyMove>][] = [
    ["ArrowDown", "full", "half"], ["ArrowDown", "half", "peek"], ["ArrowDown", "peek", "close"],
    ["End", "full", "peek"], ["End", "peek", "close"],
    ["ArrowUp", "peek", "half"], ["Home", "peek", "full"], ["Enter", "peek", null],
  ];
  for (const [k, from, want] of cases) assert.equal(keyMove(k, from), want, `${k} from ${from}`);
  const fast = FLICK_PX_PER_MS * 2, slow = FLICK_PX_PER_MS / 2;
  assert.equal(releaseMove("peek", PEEK_PX, -fast, H), "close", "a downward flick from the strip dismisses it");
  assert.equal(releaseMove("peek", PEEK_PX, -slow, H), "peek", "a nudge that is not a flick does not");
  assert.equal(releaseMove("peek", 120, fast, H), "half", "upward is unchanged");
  assert.equal(releaseMove("half", 80, -fast, H), "peek", "from half, down is still a size");
  assert.equal(releaseMove("full", 780, -fast, H), "half");
});

// ---- the mount, over a minimal fake DOM ----

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: FakeEl[] = [];
  attrs: Record<string, string> = {};
  dataset: Record<string, string> = {};
  style: Record<string, string> = { height: "" };
  className = "";
  textContent = "";
  hidden = false;
  inert = false;
  type = "";
  handlers: Record<string, Handler[]> = {};
  classList = { add: () => {}, remove: () => {}, contains: () => false };
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x === "string") this.textContent += x; else this.children.push(x); }
  prepend(...c: FakeEl[]) { this.children.unshift(...c); }
  replaceChildren(...c: FakeEl[]) { this.children = [...c]; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  setPointerCapture() {}
  focus() {}
  getBoundingClientRect() { return { x: 0, y: 0, width: 520, height: parseFloat(this.style.height || "0"), top: 0, bottom: 930, left: 0, right: 520 }; }
  querySelector(sel: string) {
    const want = sel.replace(":scope > .", "").replace(".", "");
    for (const c of this.children) if (c.className.split(" ").includes(want)) return c;
    return null;
  }
  fire(t: string, ev: Record<string, unknown> = {}) {
    for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, stopPropagation() {}, button: 0, pointerId: 1, timeStamp: 0, ...ev });
  }
  pill(list: string): FakeEl | undefined {
    for (const c of this.children) if (c.attrs["data-list"] === list) return c;
    return undefined;
  }
}

function sheetHost() {
  const host = new FakeEl("section");
  const body = new FakeEl("div");
  body.className = "sheet-body";
  host.append(body);
  return { host, body };
}

/** The card's mount plus the pills' mount over one store, with a spy client and a fake window. */
function withCard(fn: (k: {
  host: FakeEl; grab: FakeEl; head: FakeEl; close: FakeEl; home: FakeEl;
  ctx: AppContext; calls: string[]; keydown: (key: string) => void;
}) => void): void {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window, fetch: g.fetch, localStorage: g.localStorage };
  const keys: Handler[] = [];
  g.document = { createElement: (t: string) => new FakeEl(t), querySelector: () => null, body: new FakeEl("body") };
  g.window = {
    innerHeight: 1000,
    addEventListener: (t: string, fn: Handler) => { if (t === "keydown") keys.push(fn); },
    matchMedia: () => ({ matches: false }),
  };
  const calls: string[] = [];
  g.fetch = (...a: unknown[]) => { calls.push(`fetch ${JSON.stringify(a[0])}`); return Promise.reject(new Error("spy")); };
  g.localStorage = { getItem: () => null, setItem: () => {} };
  const client = new Proxy({}, { get: (_t, k) => (...a: unknown[]) => { calls.push(`${String(k)} ${JSON.stringify(a)}`); return Promise.resolve({}); } });
  const ctx = { store: createStore(initialState()), client, token: "t" } as unknown as AppContext;
  try {
    resetMapInvHome();
    // Escape is wired once per window object; this is a fresh one, so the listener is this test's.
    wireEscape(g.window as Parameters<typeof wireEscape>[0]);
    const { host } = sheetHost();
    mountFocusSheet(host as unknown as HTMLElement, ctx);
    mountInvPills(new FakeEl("aside") as unknown as HTMLElement, ctx);
    const home = new FakeEl("div");
    registerMapInvHome(home as unknown as HTMLElement);
    const [grab, head] = host.children;
    fn({
      host, grab, head, close: head.children[1], home, ctx, calls,
      keydown: (key) => { for (const fn2 of keys) fn2({ key, defaultPrevented: false, preventDefault() {} }); },
    });
  } finally {
    resetMapInvHome();
    // Nothing of this test's overlay stack may leak into the next (the stack is module state).
    while (overlays.escape()) { /* drain */ }
    g.document = saved.document; g.window = saved.window; g.fetch = saved.fetch; g.localStorage = saved.localStorage;
  }
}

test("MOUNT: the card follows the store — hidden, opened by a selection, swapped, closed", () => {
  withCard(({ host, head, ctx, calls }) => {
    assert.equal(host.hidden, true, "nothing is selected, so there is no card");
    assert.equal(host.dataset.open, "false");

    ctx.store.set(setInventoryRows({ e1: row("e1", "confirmed"), e2: row("e2", "confirmed") }, 1));
    assert.equal(host.hidden, true, "rows arriving is not a click");

    ctx.store.set(focusSignal("e1"));
    assert.equal(host.hidden, false, "a feature click put the card on screen");
    assert.equal(host.dataset.snap, "half", "at half, so what was clicked is visible");
    assert.equal(head.children[0].textContent, "Selected signal · 100.8000 MHz", "and it names it");

    ctx.store.set(focusSignal("e2"));
    assert.equal(host.hidden, false, "another feature swaps the card; it never blinks shut");

    ctx.store.set(closeCard());
    assert.equal(host.hidden, true, "a click on bare map took every pixel back");
    assert.deepEqual(calls, [], "no open, swap or close reached the client, fetch or a device route");
  });
});

test("MOUNT: the × and Escape both close the card AND clear the selection", () => {
  for (const how of ["close", "escape"] as const) {
    withCard(({ host, close, ctx, calls, keydown }) => {
      ctx.store.set(focusSignal("e1"));
      assert.equal(host.hidden, false);
      if (how === "close") close.fire("click"); else keydown("Escape");
      assert.equal(host.hidden, true, `${how} left the card on screen`);
      assert.equal(ctx.store.get().card.open, false, `${how} did not tell the store`);
      assert.deepEqual(ctx.store.get().focus, { kind: "none" }, `${how} left the feature selected`);
      assert.deepEqual(calls, [], "a dismiss reaches no route");
    });
  }
});

test("MOUNT: an inventory pill opens the card on its list, and names that list", () => {
  withCard(({ host, head, home, ctx, calls }) => {
    assert.equal(host.hidden, true);
    home.pill("candidate")!.fire("click");
    assert.equal(host.hidden, false, "the pill opened the card");
    assert.equal(host.dataset.snap, "half");
    assert.equal(ctx.store.get().inventory.tab, "candidate");
    assert.deepEqual(ctx.store.get().focus, { kind: "none" }, "a list is not a selection");
    assert.equal(head.children[0].textContent, "Candidate signals in view");
    home.pill("confirmed")!.fire("click");
    assert.equal(head.children[0].textContent, "Confirmed signals in view");
    assert.deepEqual(calls, [], "a pill reaches no route");
  });
});

test("MOUNT: a dismissed card is re-openable, and the flick-down dismiss reports to the store too", () => {
  withCard(({ host, grab, ctx }) => {
    ctx.store.set(focusSignal("e1"));
    grab.fire("keydown", { key: "End" });     // half -> peek (a size)
    assert.equal(host.hidden, false);
    assert.equal(host.dataset.snap, "peek");
    grab.fire("keydown", { key: "ArrowDown" }); // peek -> gone
    assert.equal(host.hidden, true);
    assert.equal(ctx.store.get().card.open, false);
    assert.deepEqual(ctx.store.get().focus, { kind: "none" });
    ctx.store.set(focusSignal("e1"));
    assert.equal(host.hidden, false, "the same feature opens the card again");
    assert.equal(host.dataset.snap, "half", "and at half, not at the strip it was dismissed from");
  });
});

// ---- the bare-map branch (source: node has no WebGL2 canvas to click) ----

test("the surface's click handler closes the card on BARE map only", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  const onClick = src.slice(src.indexOf("onClick: (p, e) =>"), src.indexOf("onRegionDrag:"));
  assert.ok(onClick.length > 100, "the click handler was found");
  // A pin wins, and selects (which opens the card) — it never reaches the close below.
  assert.match(onClick, /const pin = pinLayer\.pick\([^)]*\);\s*\n\s*if \(pin\) \{ selectPin\(pin\); return; \}/);
  // No mark under the pointer: the card closes, unless an annotation is there (a feature, not the
  // back of the map).
  assert.match(onClick, /if \(!hit\?\.mark\) \{[\s\S]*annotationAt\(annotations[\s\S]*store\.set\(closeCard\(\)\)[\s\S]*return;\s*\n\s*\}/);
  // A mark that IS hit focuses it (signal/selection) or selects it in Research — and closes nothing.
  const hitBranch = onClick.slice(onClick.indexOf('if (hit.mark.kind === "signal-box")'));
  assert.doesNotMatch(hitBranch, /closeCard/, "a click on a feature must not dismiss the card");
  assert.match(hitBranch, /focusSignal\(hit\.mark\.id\)/);
  assert.match(hitBranch, /focusSelection\(hit\.mark\.id\)/);
  // Hit-testing is the POLYGON, not a frequency-only or centre-only guess (docs/23 §10.6 rule 6).
  assert.match(src, /function hitAt\(x: number, y: number\)/);
  const marks = readFileSync("src/surface/marks.ts", "utf8");
  assert.match(marks, /export function markAt\([\s\S]*?if \(fHz < b\.f0Hz \|\| fHz > b\.f1Hz \|\| tNs < b\.t0Ns \|\| tNs > t1\) continue;/,
    "markAt must test all four edges of the drawn rectangle");
  // Selection is view state: the handler's writes are store actions, never a device route.
  assert.doesNotMatch(onClick, /client\.|\/api\/control\//);
});

test("the card's hidden state is in the CSS and in the markup, not only in the script", () => {
  const css = readFileSync("src/app/chrome/sheet.css", "utf8");
  // `.sheet { display: flex }` would beat the UA's `[hidden] { display: none }`.
  assert.match(css, /\.sheet\[hidden\] \{ display: none; \}/);
  const html = readFileSync("src/app/index.html", "utf8");
  assert.match(html, /<section class="sheet" data-slot="sheet"[^>]*\shidden>/,
    "the card must not paint for the frames before the script mounts");
  // Nothing may keep a strip of it on screen when it is closed.
  assert.doesNotMatch(css.replace(/\/\*[\s\S]*?\*\//g, ""), /\.sheet\[hidden\][^{]*\{[^}]*display:\s*(flex|block)/);
});

test("the minimap's clearance of the card is re-measured when the card comes and goes", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  // A hidden sheet measures a zero box, so `sheetUnder` is 0 and the strip gets its rows back —
  // but only if `fit()` runs again when openness changes (T-933's lift is otherwise measured once).
  assert.match(src, /store\.select\(\(s\) => s\.card\.open, \(\) => \{\s*\n\s*fit\(\);/);
  assert.match(src, /sr && sr\.height > 0 \? Math\.max\(0, Math\.ceil\(r\.bottom - \(sr\.bottom - PEEK_PX\)\)\) : 0/);
});

test("mountSheet's open state is never persisted: it belongs to the selection, not the profile", () => {
  const writes: string[] = [];
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = { createElement: (t: string) => new FakeEl(t), querySelector: () => null };
  g.window = { innerHeight: 1000, addEventListener() {} };
  try {
    const { host } = sheetHost();
    const ctl = mountSheet(host as unknown as HTMLElement, {
      storageKey: "k", label: "Selected", reservedPx: 170,
      storage: { getItem: () => null, setItem: (k: string, v: string) => { writes.push(`${k}=${v}`); } },
    });
    ctl.show("half");
    ctl.hide();
    ctl.show("full");
    assert.deepEqual(writes, [], "showing or hiding the card wrote a preference");
    ctl.set("full");
    assert.deepEqual(writes, ["k=full"], "sizing it is the only per-viewer preference");
  } finally {
    g.document = saved.document; g.window = saved.window;
  }
});
