// T-997 (user 2026-09-25, map-UI nit-picks): the inventory counts are two pills in the map's
// top-left chrome — not a hamburger chip at mid-height over the time ruler — and each pill opens the
// bottom sheet on its own list.
//
// No DOM under node:test, so: the counts and labels are tested as the pure functions they are; the
// mount is tested over a minimal fake DOM with a spy client (every press leaves the call list
// empty, and writes only view state); and the layout — WHERE the pills sit, and that nothing of the
// old left column survives — is read from the CSS and HTML as text (app-map-layout.test.ts's
// technique). The real-browser bounding-rect test (never over the time ruler, at 1280x800 and
// 400 px) lives in `ui/e2e/app-inv-pills.e2e.mjs`.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mountInvPills, pillAction, pillLabel, PILL_WORD, sideCounts } from "../src/app/chrome/inv-pills";
import { registerMapInvHome, resetMapInvHome } from "../src/app/chrome/inv-home";
import type { Row } from "../src/app/explore/inventory";
import type { AppContext } from "../src/app/context";
import { createStore } from "../src/app/store";
import { initialState } from "../src/app/state";
import { setInventoryRows } from "../src/app/explore/slice";
import { mounts } from "../src/app/explore";

const row = (id: string, state: string, extra: Record<string, unknown> = {}) =>
  ({ id, state, f_center_hz: 100e6, bandwidth_hz: 200e3, explanations: [], count: 1, ...extra }) as unknown as Row;

test("the pills' counts are the lists' own collection (T-389), decluttered the same way", () => {
  const rows = {
    a: row("a", "candidate"), b: row("b", "candidate"), c: row("c", "confirmed"),
    d: row("d", "candidate", { relation: { kind: "duplicate-of" } }), // not listed, not counted
    e: row("e", "deleted"),
  };
  assert.deepEqual(sideCounts(rows, null), { candidate: 2, confirmed: 1 });
  assert.deepEqual(sideCounts({}, null), { candidate: 0, confirmed: 0 });
});

test("a pill says its count and what pressing it does; the user's own short words on the face", () => {
  assert.deepEqual(PILL_WORD, { candidate: "cand", confirmed: "conf" });
  assert.equal(pillLabel("candidate", 1), "1 candidate — open the candidate list");
  assert.equal(pillLabel("confirmed", 1), "1 confirmed signal — open the confirmed list");
  assert.match(pillLabel("candidate", 3), /3 candidates/);
  assert.match(pillLabel("confirmed", 0), /0 confirmed signals/);
  // `half`, never `full`: the map stays the subject (docs/23 §10.3), and `reveal` never lowers a
  // taller state the viewer chose.
  assert.deepEqual(pillAction("candidate"), { tab: "candidate", snap: "half" });
  assert.deepEqual(pillAction("confirmed"), { tab: "confirmed", snap: "half" });
});

// ---- the mount, over a minimal fake DOM ----

type Handler = (ev: Record<string, unknown>) => void;
class FakeEl {
  children: FakeEl[] = [];
  attrs: Record<string, string> = {};
  hidden = true;
  textContent = "";
  handlers: Record<string, Handler[]> = {};
  constructor(public tag: string) {}
  append(...c: (FakeEl | string)[]) { for (const x of c) if (typeof x === "string") this.textContent += x; else this.children.push(x); }
  replaceChildren(...c: FakeEl[]) { this.children = [...c]; }
  setAttribute(k: string, v: string) { this.attrs[k] = v; }
  getAttribute(k: string) { return this.attrs[k] ?? null; }
  addEventListener(t: string, fn: Handler) { (this.handlers[t] ??= []).push(fn); }
  fire(t: string, ev: Record<string, unknown> = {}) { for (const fn of this.handlers[t] ?? []) fn({ preventDefault() {}, ...ev }); }
  find(cls: string): FakeEl | undefined {
    for (const c of this.children) {
      if ((c.attrs.class ?? "").split(" ").includes(cls)) return c;
      const d = c.find(cls); if (d) return d;
    }
    return undefined;
  }
  pill(list: string): FakeEl | undefined {
    for (const c of this.children) if (c.attrs["data-list"] === list) return c;
    return undefined;
  }
}

test("the pills fill the map's chrome home in either mount order, count, and write view state only", () => {
  const g = globalThis as Record<string, unknown>;
  const saved = { document: g.document, fetch: g.fetch };
  g.document = { createElement: (t: string) => new FakeEl(t), querySelector: () => null };
  const calls: string[] = [];
  g.fetch = (...a: unknown[]) => { calls.push(`fetch ${JSON.stringify(a[0])}`); return Promise.reject(new Error("spy")); };
  const client = new Proxy({}, { get: (_t, k) => (...a: unknown[]) => { calls.push(`${String(k)} ${JSON.stringify(a)}`); return Promise.resolve({}); } });
  const ctx = { store: createStore(initialState()), client, token: "t" } as unknown as AppContext;
  try {
    resetMapInvHome();
    const side = new FakeEl("aside");
    // The pills mount BEFORE the surface builds the cluster — the real order on a cold load.
    mountInvPills(side as unknown as HTMLElement, ctx);
    assert.deepEqual(side.children, [], "the pills are chrome, never drawn beside the lists");
    const home = new FakeEl("div");
    registerMapInvHome(home as unknown as HTMLElement);
    assert.equal(home.hidden, false, "the row stays hidden until the pills fill it");
    const cand = home.pill("candidate")!, conf = home.pill("confirmed")!;
    assert.ok(cand && conf, "both pills built");
    assert.equal(cand.find("map-pill-n")!.textContent, "0");

    ctx.store.set(setInventoryRows({ a: row("a", "candidate"), b: row("b", "candidate"), c: row("c", "confirmed") }, 1));
    assert.equal(home.pill("candidate")!.find("map-pill-n")!.textContent, "2");
    assert.equal(home.pill("confirmed")!.find("map-pill-n")!.textContent, "1");
    assert.match(home.pill("candidate")!.attrs["aria-label"], /2 candidates/);

    // A press selects that list (view state) — the sheet it opens is a real-DOM concern, proved in e2e.
    assert.equal(ctx.store.get().inventory.tab, "confirmed");
    home.pill("candidate")!.fire("click");
    assert.equal(ctx.store.get().inventory.tab, "candidate");
    home.pill("confirmed")!.fire("click");
    assert.equal(ctx.store.get().inventory.tab, "confirmed");

    // The surface remounted its cluster: the pills re-home themselves, still counting.
    const home2 = new FakeEl("div");
    registerMapInvHome(home2 as unknown as HTMLElement);
    assert.equal(home2.pill("confirmed")!.find("map-pill-n")!.textContent, "1");

    assert.deepEqual(calls, [], "a pill never reaches the client, fetch, or a device route");
  } finally {
    resetMapInvHome();
    g.document = saved.document; g.fetch = saved.fetch;
  }
});

test("the side slot moved INTO the sheet, and Explore mounts the pills from it", () => {
  const html = readFileSync("src/app/index.html", "utf8");
  assert.equal(html.split('data-slot="side"').length - 1, 1);
  const body = html.slice(html.indexOf('<div class="sheet-body">'), html.indexOf("</section>", html.indexOf('<div class="sheet-body">')));
  assert.match(body, /<aside class="side" data-slot="side"/, "the lists are not in the sheet");
  assert.match(body, /data-slot="inventory"/);
  assert.match(body, /data-slot="selections"/);
  assert.equal(mounts.side, mountInvPills);
  const src = readFileSync("src/app/chrome/inv-pills.ts", "utf8");
  assert.doesNotMatch(src, /from "\.\.\/net"|controls\/client|\bfetch\(|WebSocket|ctx\.client/, "presentation only");
  assert.doesNotMatch(src, /☰/, "no hamburger glyph");
  assert.match(src, /revealFocusSheet\(snap\)/, "a pill must open the sheet it filtered");
  // Fixed, never draggable (P3): no drag wiring in the pills.
  assert.doesNotMatch(src, /"pointer(down|move)"|onpointer|draggable"/);
});

test("layout: the pills are a row of the top-left stack, and nothing of the left column survives", () => {
  const ctl = readFileSync("src/app/chrome/map-controls.css", "utf8");
  const layout = readFileSync("src/app/chrome/map-layout.css", "utf8");
  const sheet = readFileSync("src/app/chrome/sheet.css", "utf8");
  const phone = readFileSync("src/app/chrome/phone.css", "utf8");
  // Docked top-left, under Go-to (0) and the nudge row (44) — never at mid-height, so never over
  // the time ruler's labels down each pane's left edge.
  assert.match(ctl, /\.map-inv \{ top: 88px; left: 0;/);
  assert.doesNotMatch(ctl, /\.map-inv[^{]*\{[^}]*top: 50%/);
  // The rows below it moved down by one row: the offer and the measure banner, at both breakpoints.
  assert.match(ctl, /\.map-offer \{ top: 132px; \}/);
  assert.match(ctl, /\.map-inv \{ top: 132px; \}[\s\S]*\.map-offer \{ top: 176px; \}[\s\S]*\.map-mode \{ top: 220px; \}/);
  // It fades with the rest of the cluster by being IN the cluster (`map-fade`), not by a rule of
  // its own in phone.css.
  const pills = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  assert.match(pills, /class: "map-glass map-inv map-fade"/);
  assert.doesNotMatch(phone.replace(/\/\*[\s\S]*?\*\//g, ""), /map-inv|side-chip/);
  // No floating left column, collapsed or open, anywhere.
  for (const [name, css] of [["map-layout.css", layout], ["phone.css", phone], ["map-controls.css", ctl]] as const) {
    assert.doesNotMatch(css.replace(/\/\*[\s\S]*?\*\//g, ""), /side-chip|\.side\.is-(open|collapsed)/, `${name} still styles the retired chip`);
  }
  // The lists are sheet content, ordered after what is selected (T-943) and before the drawer.
  assert.match(sheet, /\.sheet .sheet-body > \.side \{ order: 1;/);
  assert.match(sheet, /\.sheet .sheet-body > \.drawer \{ order: 2; \}/);
});
