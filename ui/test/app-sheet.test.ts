// T-803 (MAP-03): the reusable bottom sheet (`src/app/chrome/sheet.ts`). No DOM under node:test, so
// the snap model, keyboard map and persistence are tested as the pure functions they are, and the
// layout rules that make the sheet non-modal are read from the CSS/HTML as text (the technique
// app-shell.test.ts uses). The thin-client rule is asserted twice: on the source (nothing that can
// reach the backend is imported) and on a spy client handed to the Explore sheet mount through a
// minimal fake DOM — every drag, click and key leaves the spy's call list empty.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  CLEAR_GAP_PX, cycleSnap, FLICK_PX_PER_MS, keySnap, mountSheet, nearestSnap, PEEK_PX, readSnap, releaseSnap,
  reservedFor, snapHeights, SNAPS, stepSnap, writeSnap, type SheetSnap,
} from "../src/app/chrome/sheet";
import { FOCUS_SHEET_KEY, focusSheetTitle, mountFocusSheet } from "../src/app/chrome/focus-sheet";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { focusSelection, focusSignal } from "../src/app/explore/slice";

const H = snapHeights(1000, 170);

test("three snap states, ordered, from one definition", () => {
  assert.deepEqual(SNAPS, ["peek", "half", "full"]);
  assert.deepEqual(H, { peek: PEEK_PX, half: 450, full: 830 });
  // Degenerate viewports keep the order (never a `half` taller than `full`, never below peek).
  for (const vh of [0, 50, 120, 200, 400]) {
    const h = snapHeights(vh, 170);
    assert.ok(h.peek <= h.half && h.half <= h.full, `vh=${vh}: ${JSON.stringify(h)}`);
    assert.ok(h.peek === PEEK_PX);
  }
});

test("full clears the chrome above it: the fixed reserve, or the toolbar's real edge if lower", () => {
  assert.equal(reservedFor(170, null, 70), 170, "no toolbar yet: the fixed reserve");
  assert.equal(reservedFor(170, 60, 70), 170, "a toolbar above the reserve changes nothing");
  // A wrapped toolbar on a phone ends at 180 px; the sheet's bottom sits 70 px above the viewport's.
  assert.equal(reservedFor(170, 180, 70), 180 + CLEAR_GAP_PX + 70);
  const h = snapHeights(860, reservedFor(170, 180, 70));
  assert.ok(860 - 70 - h.full >= 180 + CLEAR_GAP_PX, "the full sheet's top is below the toolbar");
  assert.equal(reservedFor(170, NaN, 70), 170);
  assert.equal(reservedFor(170, 180, NaN), 170);
});

test("a released drag snaps to the nearest state; a flick moves one state its way", () => {
  assert.equal(nearestSnap(60, H), "peek");
  assert.equal(nearestSnap(300, H), "half");
  assert.equal(nearestSnap(700, H), "full");
  assert.equal(nearestSnap((PEEK_PX + 450) / 2, H), "peek", "a tie leaves less canvas covered");
  const slow = FLICK_PX_PER_MS / 2, fast = FLICK_PX_PER_MS * 2;
  assert.equal(releaseSnap("peek", 120, slow, H), "peek", "short slow drag springs back");
  assert.equal(releaseSnap("peek", 120, fast, H), "half", "short fast upward flick opens");
  assert.equal(releaseSnap("full", 780, -fast, H), "half", "downward flick lowers one state");
  assert.equal(releaseSnap("peek", 800, fast, H), "full", "never lands behind the finger");
  assert.equal(releaseSnap("half", 80, -fast, H), "peek");
  assert.equal(releaseSnap("full", 830, fast, H), "full", "clamped at the top");
});

test("click cycles, keys step and jump, other keys are not ours", () => {
  assert.deepEqual(SNAPS.map(cycleSnap), ["half", "full", "peek"]);
  assert.equal(stepSnap("peek", -1), "peek");
  assert.equal(stepSnap("full", 1), "full");
  const cases: [string, SheetSnap, SheetSnap | null][] = [
    ["ArrowUp", "peek", "half"], ["ArrowUp", "full", "full"], ["ArrowDown", "half", "peek"],
    ["Home", "peek", "full"], ["End", "full", "peek"], ["Enter", "peek", null], ["a", "half", null],
  ];
  for (const [k, s, want] of cases) assert.equal(keySnap(k, s), want, `${k} from ${s}`);
});

test("per-viewer snap state: remembered, and correct without storage", () => {
  const mem = new Map<string, string>();
  const store = { getItem: (k: string) => mem.get(k) ?? null, setItem: (k: string, v: string) => { mem.set(k, v); } };
  assert.equal(readSnap(store, "k", "peek"), "peek", "absent → default");
  writeSnap(store, "k", "full");
  assert.equal(readSnap(store, "k", "peek"), "full");
  mem.set("k", "sideways");
  assert.equal(readSnap(store, "k", "half"), "half", "an unknown value is ignored");
  const throwing = { getItem: () => { throw new Error("denied"); }, setItem: () => { throw new Error("denied"); } };
  assert.equal(readSnap(throwing, "k", "peek"), "peek");
  assert.doesNotThrow(() => writeSnap(throwing, "k", "half"));
  assert.equal(readSnap(null, "k", "peek"), "peek");
  assert.doesNotThrow(() => writeSnap(null, "k", "half"));
});

test("the peek strip says what is selected — and that nothing is, when nothing is", () => {
  assert.match(focusSheetTitle({ kind: "none" }), /nothing yet/);
  assert.equal(focusSheetTitle({ kind: "signal", id: "e1" }), "Selected signal");
  assert.equal(focusSheetTitle({ kind: "selection", id: "s1" }), "Selected region");
});

test("non-modal by construction: no backdrop, fixed to its own box, content reflows inside it", () => {
  const css = readFileSync("src/app/chrome/sheet.css", "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  assert.doesNotMatch(css, /backdrop\b(?!-filter)|::backdrop|inset:\s*0|100vh|100vw\)/, "nothing may cover the canvas outside the sheet");
  assert.doesNotMatch(css, /pointer-events/, "the sheet never reroutes pointer events");
  // T-994: the dock bar under it is retired, so the sheet sits at the map's own bottom inset.
  assert.match(css, /\.sheet \{ position: fixed;[^}]*bottom: var\(--sheet-bottom, 8px\)/, "docked to the map's bottom edge, no dock under it");
  assert.match(css, /\.sheet \.sheet-body \{ flex: 1; min-height: 0; overflow: auto; \}/, "the body reflows to the snap height");
  assert.match(css, /\.sheet \{ --sheet-gutter: 64px; right: var\(--sheet-gutter\); width: min\(520px/,
    "width-capped, docked right, clear of the zoom/FAB column (T-802)");
  assert.match(css, /@media \(max-width: 900px\) \{\s*\.sheet \{ left: 8px; width: auto; \}/, "full width on a phone");
  const html = readFileSync("src/app/index.html", "utf8");
  // T-997 added the inventory lists to the same body (ordered by `sheet.css`: focus, lists, drawer).
  assert.match(html, /<section class="sheet" data-slot="sheet"[^>]*>\s*<div class="sheet-body">/, "the sheet's body");
  const body = html.slice(html.indexOf('<div class="sheet-body">'), html.indexOf("</section>", html.indexOf('<div class="sheet-body">')));
  for (const slot of ["drawer", "focus", "side"]) {
    assert.match(body, new RegExp(`data-slot="${slot}"`), `the ${slot} panel is not in the sheet's body`);
  }
  assert.doesNotMatch(html, /<dialog|aria-modal/, "no modal anywhere in the shell");
  const src = readFileSync("src/app/chrome/sheet.ts", "utf8");
  assert.doesNotMatch(src, /showModal|aria-modal|\.focus\(\)/, "no modal, no focus trap");
});

test("thin client: the sheet modules import nothing that can reach the backend", () => {
  for (const f of ["src/app/chrome/sheet.ts", "src/app/chrome/focus-sheet.ts"]) {
    const src = readFileSync(f, "utf8");
    assert.doesNotMatch(src, /from "\.\.\/net"|controls\/client|\bfetch\(|WebSocket|ctx\.client/, f);
  }
});

// ---- the DOM wiring, over a minimal fake DOM (node has none) ----

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: FakeEl[] = [];
  parent: FakeEl | null = null;
  attrs: Record<string, string> = {};
  dataset: Record<string, string> = {};
  style: Record<string, string> = {};
  className = "";
  classes = new Set<string>();
  classList = { add: (c: string) => this.classes.add(c), remove: (c: string) => this.classes.delete(c), contains: (c: string) => this.classes.has(c) };
  textContent = "";
  inert = false;
  type = "";
  handlers: Record<string, Handler[]> = {};
  constructor(public tag: string) {}
  append(...c: FakeEl[]) { for (const x of c) { x.parent = this; this.children.push(x); } }
  prepend(...c: FakeEl[]) { for (const x of c) x.parent = this; this.children.unshift(...c); }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  querySelector(sel: string) { return sel === ":scope > .sheet-body" ? this.children.find((c) => c.className === "sheet-body") ?? null : null; }
  // The sheet is docked 70 px above the bottom of a 1000 px viewport (`--sheet-bottom`).
  getBoundingClientRect() { return { height: parseFloat(this.style.height ?? "56"), bottom: 930 }; }
  setPointerCapture() {}
  fire(t: string, ev: Record<string, unknown> = {}) { for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, button: 0, pointerId: 1, ...ev }); }
}

function withFakeDom<T>(vh: number, fn: () => T): T {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window };
  g.document = { createElement: (t: string) => new FakeEl(t), querySelector: () => null };
  g.window = { innerHeight: vh, addEventListener() {} };
  try { return fn(); } finally { g.document = saved.document; g.window = saved.window; }
}

function spyCtx() {
  const calls: string[] = [];
  const client = new Proxy({}, { get: (_t, k) => (...a: unknown[]) => { calls.push(`${String(k)} ${JSON.stringify(a)}`); return Promise.resolve({}); } });
  const origFetch = globalThis.fetch;
  globalThis.fetch = ((...a: unknown[]) => { calls.push(`fetch ${JSON.stringify(a[0])}`); return Promise.reject(new Error("spy")); }) as typeof fetch;
  const ctx = { store: createStore(initialState()), client, token: "t" } as unknown as AppContext;
  return { ctx, calls, restore: () => { globalThis.fetch = origFetch; } };
}

function sheetHost() {
  const host = new FakeEl("section");
  const body = new FakeEl("div");
  body.className = "sheet-body";
  const focus = new FakeEl("aside");
  body.append(focus);
  host.append(body);
  return { host, body, focus };
}

test("the mount wraps, never replaces, the hosted slot; drag/click/keys snap and persist", () => {
  withFakeDom(1000, () => {
    const mem = new Map<string, string>();
    const storage = { getItem: (k: string) => mem.get(k) ?? null, setItem: (k: string, v: string) => { mem.set(k, v); } };
    const { host, body, focus } = sheetHost();
    const ctl = mountSheet(host as unknown as HTMLElement, { storageKey: "s", label: "Selected", reservedPx: 170, storage });
    const [grab, head] = host.children;
    assert.equal(host.children[2], body, "the body stays in place");
    assert.equal(body.children[0], focus, "the hosted slot is untouched");
    assert.equal(host.dataset.snap, "peek");
    assert.equal(host.style.height, `${PEEK_PX}px`);
    assert.equal(body.inert, true, "collapsed content is out of the tab order");

    grab.fire("click");
    assert.equal(ctl.get(), "half");
    assert.equal(host.style.height, "450px");
    assert.equal(body.inert, false);
    assert.equal(mem.get("s"), "half", "the viewer's choice is remembered");

    grab.fire("keydown", { key: "ArrowUp" });
    assert.equal(ctl.get(), "full");
    grab.fire("keydown", { key: "End" });
    assert.equal(ctl.get(), "peek");

    // Drag up past the half midpoint, slowly: lands on the nearest state, and the click the browser
    // fires after the pointerup does not ALSO cycle it.
    grab.fire("pointerdown", { clientY: 900, timeStamp: 0 });
    grab.fire("pointermove", { clientY: 700, timeStamp: 1000 });
    grab.fire("pointermove", { clientY: 500, timeStamp: 2000 });
    grab.fire("pointerup");
    grab.fire("click");
    assert.equal(ctl.get(), "half", `dragged to ${host.style.height}`);

    // A fast upward flick from half goes to full even though it stopped short of the midpoint.
    grab.fire("pointerdown", { clientY: 500, timeStamp: 3000 });
    grab.fire("pointermove", { clientY: 480, timeStamp: 3010 });
    grab.fire("pointerup");
    assert.equal(ctl.get(), "full");

    // The title strip opens a collapsed sheet.
    ctl.set("peek");
    head.fire("click");
    assert.equal(ctl.get(), "half");

    // docs/23 §10.6 P1: a visible, labelled dismiss collapses the open sheet to its strip (its
    // pixels go back to the map) and is itself hidden once collapsed.
    const close = head.children[1];
    assert.equal(close.className, "sheet-close");
    assert.match(close.getAttribute("aria-label") ?? "", /^Close Selected sheet/);
    assert.equal((close as unknown as { hidden: boolean }).hidden, false);
    close.fire("click", { stopPropagation() {} });
    assert.equal(ctl.get(), "peek");
    assert.equal(mem.get("s"), "peek");
    assert.equal((close as unknown as { hidden: boolean }).hidden, true);
    ctl.set("half");

    // reveal() raises but never lowers, and is not the viewer's stored choice.
    ctl.set("peek");
    ctl.reveal("half");
    assert.equal(ctl.get(), "half");
    assert.equal(mem.get("s"), "peek");
    ctl.set("full");
    ctl.reveal("half");
    assert.equal(ctl.get(), "full");

    // A remount reads the remembered state; unavailable storage still renders, at the default.
    const again = sheetHost();
    assert.equal(mountSheet(again.host as unknown as HTMLElement, { storageKey: "s", label: "x", storage }).get(), "full");
    const none = sheetHost();
    const c3 = mountSheet(none.host as unknown as HTMLElement, { storageKey: "s", label: "x", storage: null });
    assert.equal(c3.get(), "peek");
    assert.equal(none.host.style.height, `${PEEK_PX}px`);
  });
});

test("full is re-bounded by the chrome it clears, as that chrome moves", () => {
  withFakeDom(1000, () => {
    let toolbarBottom: number | null = null;
    const { host } = sheetHost();
    const ctl = mountSheet(host as unknown as HTMLElement,
      { storageKey: "s", label: "x", reservedPx: 170, storage: null, clearOf: () => toolbarBottom });
    ctl.set("full");
    assert.equal(host.style.height, "830px", "no toolbar yet: the fixed reserve");
    toolbarBottom = 250; // the toolbar wrapped to more rows
    ctl.relayout();
    assert.equal(host.style.height, `${1000 - (250 + CLEAR_GAP_PX + 70)}px`);
    assert.ok(930 - parseFloat(host.style.height) >= 250 + CLEAR_GAP_PX, "top edge below the toolbar");
    assert.equal(ctl.get(), "full", "a relayout never changes the state");
  });
});

test("SPY CLIENT: selecting raises the sheet, and no sheet gesture reaches any route", () => {
  withFakeDom(1000, () => {
    const g = globalThis as Record<string, unknown>;
    const savedLs = g.localStorage;
    const mem = new Map<string, string>();
    g.localStorage = { getItem: (k: string) => mem.get(k) ?? null, setItem: (k: string, v: string) => { mem.set(k, v); } };
    const spy = spyCtx();
    try {
      const { host } = sheetHost();
      mountFocusSheet(host as unknown as HTMLElement, spy.ctx);
      const [grab, head] = host.children;
      assert.equal(host.dataset.snap, "peek");
      assert.match(head.children[0].textContent, /nothing yet/);

      spy.ctx.store.set(focusSignal("e1"));
      assert.equal(host.dataset.snap, "half", "a new selection opens a collapsed sheet");
      assert.equal(head.children[0].textContent, "Selected signal");
      assert.equal(mem.get(FOCUS_SHEET_KEY), undefined, "…without overwriting the viewer's preference");

      grab.fire("keydown", { key: "End" });
      spy.ctx.store.set(focusSignal("e1"));
      assert.equal(host.dataset.snap, "peek", "re-asserting the same focus does not reopen it");
      spy.ctx.store.set(focusSelection("s1"));
      assert.equal(host.dataset.snap, "half");

      for (const k of ["ArrowUp", "ArrowDown", "Home", "End"]) grab.fire("keydown", { key: k });
      grab.fire("click");
      grab.fire("pointerdown", { clientY: 900, timeStamp: 0 });
      grab.fire("pointermove", { clientY: 400, timeStamp: 50 });
      grab.fire("pointerup");
      head.fire("click");
      assert.deepEqual(spy.calls, [], "the sheet never reaches the client, fetch, or a device route");
    } finally {
      spy.restore();
      g.localStorage = savedLs;
    }
  });
});
