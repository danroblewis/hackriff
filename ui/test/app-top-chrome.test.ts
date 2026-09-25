// T-993 (MMAP): the app-shell top bar retires over the map. The browser tier
// (`ui/e2e/app-top-chrome.e2e.mjs`) proves the layout at 1280 and 400 px and clicks every former bar
// action; this proves, with no browser, the wiring a later edit could quietly cut:
//  1. **Every control the bar carries has a home on the map** — a new bar button added without one
//     would vanish from Explore (the bar is `display: none` there). Brand and the bar's Go-to are the
//     two deliberate exceptions: the map has its own Go-to (T-802), and the brand is not a control.
//  2. **The controls are MOVED, not copied**, into the homes while Explore shows and back into the
//     bar in their original order for Decode/History — one element per id, listeners intact.
//  3. **The wiring exists**: the shell places on every mode change; the cluster registers its homes.
//  4. **Nothing here reaches a route.**
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { TOP_CHROME_PARTS, placeTopChrome, registerMapHome } from "../src/app/chrome/top-chrome";

const src = (f: string) => readFileSync(f, "utf8");
const html = src("src/app/index.html");
const header = /<header class="bar">([\s\S]*?)<\/header>/.exec(html)![1];

test("every control in the retired bar has a home on the map (brand and the bar's own Go-to excepted)", () => {
  // Top-level children of the header: one per line, indented four spaces.
  const kids = [...header.matchAll(/^ {4}<(\w+)([^>]*)>/gm)].map((m) => m[2]);
  assert.ok(kids.length >= 9, `header children not found: ${kids.length}`);
  const homed = (attrs: string) => TOP_CHROME_PARTS.some(([sel]) =>
    sel.startsWith("#") ? attrs.includes(`id="${sel.slice(1)}"`)
      : sel.startsWith(".") ? new RegExp(`class="[^"]*\\b${sel.slice(1)}\\b`).test(attrs)
        : attrs.includes(sel.replace(/^\[|\]$/g, "").replace("=", '="') + '"'));
  const orphans = kids.filter((a) => !homed(a) && !/class="(brand|bar-spacer|goto)"/.test(a));
  assert.deepEqual(orphans, [], "a bar control with no home would disappear from Explore");
  // And each part names something that really is in the bar.
  for (const [sel] of TOP_CHROME_PARTS) {
    assert.ok(kids.some((a) => homed(a) && (sel.startsWith("#") ? a.includes(`id="${sel.slice(1)}"`) : true)), `${sel} is not in the bar`);
  }
  const homes = new Set(TOP_CHROME_PARTS.map(([, w]) => w));
  assert.deepEqual([...homes].sort(), ["more", "nudge", "review", "status"]);
  assert.equal(TOP_CHROME_PARTS.find(([s]) => s === "#theme-btn")?.[1], "more", "Theme lives in the ⋯ menu, not on the map face");
  assert.equal(TOP_CHROME_PARTS.find(([s]) => s === "#review-btn")?.[1], "review", "Review sits in the top-right cluster");
  assert.equal(TOP_CHROME_PARTS.find(([s]) => s === "[data-slot=nudge]")?.[1], "nudge", "the nudges sit under Go-to");
});

// A minimal DOM: enough of Node/Element for `place()` (querySelector by id/class/attr, before, after,
// append, parentElement, previousSibling, matches, childElementCount, hidden).
class FakeNode {
  parent: FakeEl | null = null;
  get parentElement() { return this.parent; }
  get previousSibling(): FakeNode | null {
    const k = this.parent?.kids; if (!k) return null;
    const i = k.indexOf(this); return i > 0 ? k[i - 1] : null;
  }
  remove() { if (this.parent) { this.parent.kids.splice(this.parent.kids.indexOf(this), 1); this.parent = null; } }
  before(n: FakeNode) { const p = this.parent!; n.remove(); p.kids.splice(p.kids.indexOf(this), 0, n); n.parent = p; }
  after(n: FakeNode) { const p = this.parent!; n.remove(); p.kids.splice(p.kids.indexOf(this) + 1, 0, n); n.parent = p; }
}
class FakeEl extends FakeNode {
  kids: FakeNode[] = [];
  hidden = false;
  constructor(public tag: string, public attrs: Record<string, string> = {}) { super(); }
  append(n: FakeNode) { n.remove(); this.kids.push(n); n.parent = this; }
  get childElementCount() { return this.kids.filter((k) => k instanceof FakeEl).length; }
  matches(sel: string): boolean {
    if (sel === ".app > .bar") return this.tag === "header";
    if (sel.startsWith("#")) return this.attrs.id === sel.slice(1);
    if (sel.startsWith(".")) return (this.attrs.class ?? "").split(" ").includes(sel.slice(1));
    const m = /^\[([\w-]+)=(\w+)\]$/.exec(sel);
    return !!m && this.attrs[m[1]] === m[2];
  }
  *walk(): Generator<FakeEl> { for (const k of this.kids) if (k instanceof FakeEl) { yield k; yield* k.walk(); } }
  querySelector(sel: string): FakeEl | null { for (const e of this.walk()) if (e.matches(sel)) return e; return null; }
}
class FakeComment extends FakeNode {}

test("Explore moves the controls into the map's homes; Decode/History put them back in order", () => {
  const body = new FakeEl("body");
  const bar = new FakeEl("header");
  body.append(bar);
  const ids = ["modes", "device", "rec-pill", "readouts", "conn", "nudge", "review-btn", "theme-btn"];
  const parts = {
    modes: new FakeEl("div", { class: "modes" }), device: new FakeEl("div", { id: "device" }),
    "rec-pill": new FakeEl("div", { id: "rec-pill" }), readouts: new FakeEl("div", { class: "readouts" }),
    conn: new FakeEl("span", { id: "conn" }), nudge: new FakeEl("div", { "data-slot": "nudge" }),
    "review-btn": new FakeEl("button", { id: "review-btn" }), "theme-btn": new FakeEl("button", { id: "theme-btn" }),
  } as Record<string, FakeEl>;
  const brand = new FakeEl("div", { class: "brand" });
  bar.append(brand);
  for (const id of ids) bar.append(parts[id]);
  const home = { status: new FakeEl("div"), nudge: new FakeEl("div"), review: new FakeEl("span"), more: new FakeEl("div") };
  const ctl = new FakeEl("div");
  for (const h of Object.values(home)) ctl.append(h);
  body.append(ctl);

  const g = globalThis as { document?: unknown };
  const saved = g.document;
  g.document = { querySelector: (s: string) => body.querySelector(s), createComment: () => new FakeComment() };
  try {
    placeTopChrome(true);
    registerMapHome(home as unknown as Parameters<typeof registerMapHome>[0]);
    assert.deepEqual(home.status.kids, [parts.modes, parts.device, parts["rec-pill"], parts.readouts, parts.conn]);
    assert.deepEqual(home.nudge.kids, [parts.nudge]);
    assert.deepEqual(home.review.kids, [parts["review-btn"]]);
    assert.deepEqual(home.more.kids, [parts["theme-btn"]]);
    assert.equal(bar.childElementCount, 1, "only the brand stays in the (hidden) bar");
    assert.equal(home.status.hidden, false);
    assert.equal(home.nudge.hidden, false);

    placeTopChrome(false); // Decode
    const order = bar.kids.filter((k) => k instanceof FakeEl);
    assert.deepEqual(order, [brand, ...ids.map((i) => parts[i])], "the bar gets every control back, in its own order");
    assert.equal(home.status.hidden, true, "an empty pill is not left floating");
    assert.equal(home.nudge.hidden, true);

    placeTopChrome(true); // and back again — moved, never duplicated
    assert.equal(home.status.childElementCount, 5);
    assert.equal(bar.childElementCount, 1);
  } finally {
    placeTopChrome(false);
    g.document = saved;
  }
});

test("the shell places the bar's controls on every mode change, and the cluster registers its homes", () => {
  const shell = src("src/app/shell.ts");
  assert.match(shell, /store\.select\(\(s\) => s\.mode, \(mode\) => \{[\s\S]*?placeTopChrome\(mode === "explore"\)/);
  const ctl = src("src/app/chrome/map-controls.ts");
  assert.match(ctl, /registerMapHome\(\{ status: statusHome, nudge: nudgeHome, review: reviewHome, more: moreBody \}\)/);
  // The ⋯ menu is an overlay: a visible close and Esc through the one stack (T-900).
  assert.match(ctl, /trackOverlay\("more-menu"/);
  assert.match(ctl, /map-more-close/);
});

test("nothing in the top chrome names a route", () => {
  for (const f of ["src/app/chrome/top-chrome.ts"]) assert.doesNotMatch(src(f), /\/api\//, `${f} names a route`);
});
