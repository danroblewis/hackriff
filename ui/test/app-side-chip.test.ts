// T-895 (user P1): the left inventory column collapses to a chip by default, on every width, and
// opens to an overlay with a visible close. No DOM under node:test, so the counts, label and the
// narrow-screen placement are tested as the pure functions they are; the default-collapsed wiring
// and the thin-client rule are tested over a minimal fake DOM with a spy client (every press leaves
// the call list empty); and the layout rules are read from the CSS/HTML as text (the technique
// app-map-layout.test.ts uses). The real-browser hit test lives in `ui/e2e/app-side-chip.e2e.mjs`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  mountSideChip, SIDE_CHIP_MAX_PX, SIDE_TOP_GAP_PX, sideChipLabel, sideCounts, sideTopPx,
} from "../src/app/chrome/side-chip";
import type { Row } from "../src/app/explore/inventory";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { setInventoryRows } from "../src/app/explore/slice";
import { mounts } from "../src/app/explore";

const row = (id: string, state: string, extra: Record<string, unknown> = {}) =>
  ({ id, state, f_center_hz: 100e6, bandwidth_hz: 200e3, explanations: [], count: 1, ...extra }) as unknown as Row;

test("the chip's counts are the lists' own collection (T-389), decluttered the same way", () => {
  const rows = {
    a: row("a", "candidate"), b: row("b", "candidate"), c: row("c", "confirmed"),
    d: row("d", "candidate", { relation: { kind: "duplicate-of" } }), // not listed, not counted
    e: row("e", "deleted"),
  };
  assert.deepEqual(sideCounts(rows, null), { candidate: 2, confirmed: 1 });
  assert.deepEqual(sideCounts({}, null), { candidate: 0, confirmed: 0 });
  assert.equal(sideChipLabel({ candidate: 1, confirmed: 0 }),
    "Signal lists: 1 candidate, 0 confirmed — open the lists over the map");
  assert.match(sideChipLabel({ candidate: 3, confirmed: 5 }), /3 candidates, 5 confirmed/);
});

test("narrow: the open overlay starts below the lowest top-chrome edge, ignoring what is unbuilt", () => {
  assert.equal(sideTopPx([48, 180.2, 230]), 230 + SIDE_TOP_GAP_PX);
  assert.equal(sideTopPx([48, null, undefined, NaN]), 48 + SIDE_TOP_GAP_PX);
  assert.equal(sideTopPx([180.2]), 181 + SIDE_TOP_GAP_PX, "rounded up, never into the chrome");
  assert.equal(sideTopPx([]), null, "nothing measurable: the CSS fallback stands");
});

// ---- the mount, over a minimal fake DOM ----

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: FakeEl[] = [];
  attrs: Record<string, string> = {};
  classes = new Set<string>();
  classList = {
    add: (c: string) => this.classes.add(c), remove: (c: string) => this.classes.delete(c),
    contains: (c: string) => this.classes.has(c),
    toggle: (c: string, on?: boolean) => { const v = on ?? !this.classes.has(c); if (v) this.classes.add(c); else this.classes.delete(c); return v; },
  };
  style = { props: {} as Record<string, string>, setProperty(k: string, v: string) { this.props[k] = v; } };
  textContent = "";
  handlers: Record<string, Handler[]> = {};
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x === "string") this.textContent += x; else this.children.push(x); }
  prepend(...c: FakeEl[]) { this.children.unshift(...c); }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  focus() {}
  fire(t: string, ev: Record<string, unknown> = {}) { for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, ...ev }); }
  find(cls: string): FakeEl | undefined {
    for (const c of this.children) {
      if ((c.attrs.class ?? "").split(" ").includes(cls)) return c;
      const d = c.find(cls); if (d) return d;
    }
    return undefined;
  }
}

test("default collapsed; the chip opens the lists, the close and Escape put the map back; no route", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, window: g.window, fetch: g.fetch };
  const winHandlers: Record<string, Handler[]> = {};
  const bottoms: Record<string, number> = { ".app > .bar": 48, ".sf-bar": 150, ".map-goto": 210 };
  g.document = {
    createElement: (t: string) => new FakeEl(t),
    querySelector: (sel: string) => sel in bottoms ? { getBoundingClientRect: () => ({ height: 30, bottom: bottoms[sel] }) } : null,
  };
  g.window = { addEventListener: (t: string, fn: Handler) => { (winHandlers[t] ??= []).push(fn); } };
  const calls: string[] = [];
  g.fetch = (...a: unknown[]) => { calls.push(`fetch ${JSON.stringify(a[0])}`); return Promise.reject(new Error("spy")); };
  const client = new Proxy({}, { get: (_t, k) => (...a: unknown[]) => { calls.push(`${String(k)} ${JSON.stringify(a)}`); return Promise.resolve({}); } });
  const ctx = { store: createStore(initialState()), client, token: "t" } as unknown as AppContext;
  try {
    const side = new FakeEl("aside");
    const inv = new FakeEl("div"); inv.attrs.class = "side-inv";
    side.append(inv);
    mountSideChip(side as unknown as HTMLElement, ctx);
    assert.ok(side.classes.has("is-collapsed") && !side.classes.has("is-open"), "collapsed is the default");
    assert.ok(side.children.includes(inv), "the inventory slot is wrapped, never replaced");
    const chip = side.find("side-chip")!, close = side.find("side-close")!;
    assert.ok(chip && close);
    assert.equal(chip.attrs["aria-expanded"], "false");
    assert.equal(side.find("cand")!.textContent, "0");

    ctx.store.set(setInventoryRows({ a: row("a", "candidate"), b: row("b", "candidate"), c: row("c", "confirmed") }, 1));
    assert.equal(side.find("cand")!.textContent, "2");
    assert.equal(side.find("conf")!.textContent, "1");
    assert.match(chip.attrs["aria-label"], /2 candidates, 1 confirmed/);

    chip.fire("click");
    assert.ok(side.classes.has("is-open") && !side.classes.has("is-collapsed"));
    assert.equal(chip.attrs["aria-expanded"], "true");
    assert.equal(side.style.props["--side-top"], `${210 + SIDE_TOP_GAP_PX}px`, "placed below Go-to on narrow screens");
    close.fire("click");
    assert.ok(side.classes.has("is-collapsed") && !side.classes.has("is-open"), "the close puts the map back");

    chip.fire("click");
    for (const fn of winHandlers.keydown ?? []) fn({ key: "Escape" });
    assert.ok(side.classes.has("is-collapsed"), "Escape closes it too");
    for (const fn of winHandlers.resize ?? []) fn({});
    assert.deepEqual(calls, [], "the chip never reaches the client, fetch, or a device route");
  } finally {
    g.document = saved.document; g.window = saved.window; g.fetch = saved.fetch;
  }
});

test("the side slot exists once and Explore mounts the chip on it", () => {
  const html = readFileSync("src/app/index.html", "utf8");
  assert.equal(html.split('data-slot="side"').length - 1, 1);
  assert.match(html, /<aside class="side" data-slot="side"/);
  assert.equal(mounts.side, mountSideChip);
  const src = readFileSync("src/app/chrome/side-chip.ts", "utf8");
  assert.doesNotMatch(src, /from "\.\.\/net"|controls\/client|\bfetch\(|WebSocket|ctx\.client/, "presentation only");
});

test("layout: collapsed takes no map but the chip (≤ 56 px); open starts below the top chrome", () => {
  const css = readFileSync("src/app/chrome/map-layout.css", "utf8");
  const ctl = readFileSync("src/app/chrome/map-controls.css", "utf8");
  assert.equal(SIDE_CHIP_MAX_PX, 56);
  assert.match(css, /\.side-chip \{[^}]*max-height: 56px/);
  // Collapsed hides everything but the chip — display: none, not a fade.
  assert.match(css, /\.side\.is-collapsed > :not\(\.side-chip\)[^{]*\{ display: none; \}/);
  assert.match(css, /\.side\.is-open > \.side-chip \{ display: none; \}|\.side\.is-open > \.side-chip[^{]*\{ display: none; \}/);
  assert.doesNotMatch(css, /\.side[^{]*\{[^}]*opacity/, "collapsing is not a fade");
  // The open column starts below the measured top chrome (bar, toolbar, Go-to), so neither the
  // toolbar nor the floating cluster gives up an inset for it (the inset squeezed T-528's toolbar).
  assert.match(css, /\.side\.is-open \{ top: var\(--side-top/);
  assert.match(css, /\.sf-bar \{ padding-left: 12px;/);
  assert.match(ctl, /left: var\(--panel-gap, 8px\);/);
  // Narrow open overlay: clear of the zoom/FAB gutter and above the sheet's peek strip.
  assert.match(css, /\.side\.is-open \{ width: auto; right: 64px; bottom: calc\(70px \+ 56px \+ 8px\)/);
  // Fixed, never draggable (P3): no drag wiring in the chip.
  const src = readFileSync("src/app/chrome/side-chip.ts", "utf8");
  assert.doesNotMatch(src, /"pointer(down|move)"|onpointer|draggable"/);
});
