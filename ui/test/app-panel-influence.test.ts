// T-899 (docs/23 §10.6 rule 4, folded into MAP-25/T-825): SIZE IS INVERSELY PROPORTIONAL TO
// INFLUENCE. A big panel - the bottom sheet (Selected / Explore), the Explore drawer, the Research
// slide-in, the left inventory column - may only add or remove marks, select/highlight one, or
// shift coordinates slightly. Anything that changes the map in a MAJOR way - a device route (the
// gated DeviceAction path: retune, listen, record, sweep) or a view jump (jump/fly/centre-on, zoom,
// follow live, restore a saved view) - lives on a SMALL explicit control: a `<button>` (a per-row
// button, a compact action cluster, a menu item), never on the bare click/key/pointer of a row or a
// panel body.
//
// No DOM under node:test and the panels are many modules (and more are coming: T-814's drawer,
// T-821's Research slide-in), so the guard is structural and FAILS CLOSED: it reads every module
// under `src/app/` except the few named in `NOT_A_PANEL` (each with its reason), finds every
// interaction handler bound to an element, and for each one bound to anything that is not a small
// control, asserts the handler - including the same-file functions it calls, two levels deep -
// names no device route and no view jump. A new panel file is therefore covered the day it lands,
// without anyone remembering to list it. The guard's own teeth are proven on injected sources
// below: a row click that jumps the view, a row click that retunes, a panel-body listener, and a
// jump hidden behind a same-file helper all go red; the same calls on a per-row `<button>` stay
// green.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";

// ---- the scanner ----

/** Interaction events a viewer's bare click/tap/key on an element raises. `resize`, `scroll`,
 * `input`, `change` (a form control's own value) and pointer *moves* are not a press. */
const PRESS_EVENTS = ["click", "dblclick", "auxclick", "contextmenu", "keydown", "keyup", "pointerdown", "pointerup",
  "mousedown", "mouseup", "touchstart", "touchend"];

/** Small explicit controls. `role="button"` on a row does NOT count: a row is a row. */
const SMALL_TAGS = new Set(["button", "input", "select", "textarea"]);

/** A device route (the gated DeviceAction path, docs/23 §4/§11 rule 1) or a view jump (a MAJOR map
 * change, §10.6 rule 4). Identifier-shaped, so `jumpToView`, `flyTo`, `zoomBy`, `followLive`,
 * `retuneTo`, `restoreView`, `centreOn` and a literal `/api/control/...` or `/ws/open/listen` all
 * hit. Deliberately broad: a false hit is a one-line rename or a move onto a button, a miss is a
 * big panel that moves the map. */
const MAJOR = new RegExp([
  String.raw`\/api\/control\b`, String.raw`\/ws\/open\b`,
  // Identifiers, matched anywhere inside a camelCase name (`offerRetune`, `paneJumpTo`), reported whole.
  ...["DeviceAction", "[rR]etune", "tune[A-Z]", "[sS]etCent(?:re|er)", "[jJ]ump", "[fF]ly(?:To|By|[A-Z])", "[pP]an(?:To|By)",
    "[cC]ent(?:re|er)On", "[zZ]oom", "[fF]ollowLive", "[sS]etView", "[rR]estore\\w*View", "[gG]o[tT]o[A-Z]", "viewChanged",
    "[sS]etPane", "[fF]reezePane"].map((x) => String.raw`\b\w*?` + x + String.raw`\w*`),
].join("|"));

/** Index just past the bracket closing the one at `open` (`{`, `(` or `[`), skipping strings,
 * template literals and comments. -1 when unbalanced. */
export function closeOf(src: string, open: number): number {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    const c = src[i];
    if (c === "/" && src[i + 1] === "/") { i = src.indexOf("\n", i); if (i < 0) return -1; continue; }
    if (c === "/" && src[i + 1] === "*") { i = src.indexOf("*/", i + 2) + 1; if (i <= 0) return -1; continue; }
    if (c === '"' || c === "'" || c === "`") {
      for (i++; i < src.length && src[i] !== c; i++) if (src[i] === "\\") i++;
      continue;
    }
    if (c === "{" || c === "(" || c === "[") depth++;
    else if (c === "}" || c === ")" || c === "]") { depth--; if (depth === 0) return i + 1; }
  }
  return -1;
}

/** From `from`, the expression running to the next depth-0 `,` or the enclosing close bracket. */
function exprFrom(src: string, from: number): string {
  let depth = 0;
  for (let i = from; i < src.length; i++) {
    const c = src[i];
    if (c === '"' || c === "'" || c === "`") { for (i++; i < src.length && src[i] !== c; i++) if (src[i] === "\\") i++; continue; }
    if (c === "{" || c === "(" || c === "[") depth++;
    else if (c === "}" || c === ")" || c === "]") { if (depth === 0) return src.slice(from, i); depth--; }
    else if (c === "," && depth === 0) return src.slice(from, i);
  }
  return src.slice(from);
}

/** Bodies of same-file functions, methods and arrow consts, by name (a name defined twice keeps
 * both bodies, joined: the guard would rather over-read than miss). */
function definitions(src: string): Map<string, string> {
  const defs = new Map<string, string>();
  const add = (name: string, open: number) => {
    const end = closeOf(src, open);
    if (end > 0) defs.set(name, (defs.get(name) ?? "") + src.slice(open, end));
  };
  for (const m of src.matchAll(/\bfunction\s+(\w+)\s*(?:<[^>]*>)?\s*\(/g)) {
    const params = closeOf(src, m.index! + m[0].length - 1);
    const brace = params > 0 ? src.indexOf("{", params) : -1;
    if (brace > 0) add(m[1], brace);
  }
  for (const m of src.matchAll(/\b(?:const|let)\s+(\w+)\s*=\s*(?:async\s*)?\(/g)) {
    const params = closeOf(src, m.index! + m[0].length - 1);
    const arrow = params > 0 ? /^\s*(?::[^=]*)?=>\s*/.exec(src.slice(params)) : null;
    if (!arrow) continue;
    const at = params + arrow[0].length;
    if (src[at] === "{") add(m[1], at); else defs.set(m[1], (defs.get(m[1]) ?? "") + exprFrom(src, at));
  }
  // Class members: `name(args) {` / `private async name(args): T {` / `name = (args) => ...`.
  for (const m of src.matchAll(/^\s*(?:(?:private|public|protected|readonly|static|async)\s+)*(\w+)\s*\(/gm)) {
    if (["if", "for", "while", "switch", "catch", "return", "function"].includes(m[1])) continue;
    const params = closeOf(src, m.index! + m[0].length - 1);
    const rest = params > 0 ? /^\s*(?::[^{;=]*)?\{/.exec(src.slice(params)) : null;
    if (rest) add(m[1], params + rest[0].length - 1);
  }
  for (const m of src.matchAll(/^\s*(?:(?:private|public|protected|readonly|static)\s+)*(\w+)\s*=\s*(?:async\s*)?\(/gm)) {
    const params = closeOf(src, m.index! + m[0].length - 1);
    const arrow = params > 0 ? /^\s*(?::[^=]*)?=>\s*/.exec(src.slice(params)) : null;
    if (arrow) defs.set(m[1], (defs.get(m[1]) ?? "") + exprFrom(src, params + arrow[0].length));
  }
  return defs;
}

/** The handler's text plus the bodies of the same-file functions it calls, `depth` levels deep. */
function expand(handler: string, defs: Map<string, string>, depth = 2): string {
  let text = handler;
  const seen = new Set<string>();
  let frontier = handler;
  for (let d = 0; d < depth; d++) {
    let next = "";
    for (const m of frontier.matchAll(/\b(\w+)\s*\(/g)) {
      const body = defs.get(m[1]);
      if (body && !seen.has(m[1])) { seen.add(m[1]); next += "\n" + body; }
    }
    if (!next) break;
    text += next;
    frontier = next;
  }
  return text;
}

export interface Binding { file: string; line: number; tag: string; event: string; hit: string }

const lineOf = (src: string, i: number) => src.slice(0, i).split("\n").length;

/** The tag an identifier was built with in this file (`const x = h("div", …)` or
 * `document.createElement("div")`), or null when it cannot be told - a mount's host element, a
 * parameter, a query result - which the guard treats as a panel body (fail closed). */
function tagOf(src: string, ident: string): string | null {
  const name = ident.split(".").pop()!;
  const re = new RegExp(String.raw`\b${name}\s*=\s*(?:h\(\s*"([a-z0-9-]+)"|document\.createElement\(\s*"([a-z0-9-]+)"\))`);
  const m = re.exec(src);
  return m ? (m[1] ?? m[2]) : null;
}

/** Every press handler bound to a non-small element whose (expanded) text names a MAJOR action. */
export function bigPanelViolations(file: string, src: string): Binding[] {
  const defs = definitions(src);
  const out: Binding[] = [];
  const check = (tag: string, event: string, handler: string, at: number) => {
    if (SMALL_TAGS.has(tag) || !PRESS_EVENTS.includes(event)) return;
    const m = MAJOR.exec(expand(handler, defs));
    if (m) out.push({ file, line: lineOf(src, at), tag, event, hit: m[0] });
  };
  // `h("tag", { …, onclick: handler, … })` - the app's one element builder (src/app/dom.ts).
  for (const m of src.matchAll(/\bh\(\s*"([a-z0-9-]+)"\s*,\s*\{/g)) {
    const open = m.index! + m[0].length - 1;
    const end = closeOf(src, open);
    if (end < 0) continue;
    const attrs = src.slice(open, end);
    // Keys at the object's own depth only (a nested h() in a child is its own match).
    for (const k of attrs.matchAll(/\bon([a-z]+)\s*:/g)) {
      const pre = attrs.slice(0, k.index!);
      if (closeOfDepth(pre) !== 1) continue;
      check(m[1], k[1], exprFrom(attrs, k.index! + k[0].length), open + k.index!);
    }
  }
  // `el.addEventListener("click", handler)` and `el.onclick = handler`.
  for (const m of src.matchAll(/([\w$.]+)\.addEventListener\(\s*"(\w+)"\s*,/g)) {
    if (/^(?:window|document|globalThis)$/.test(m[1])) continue; // global shortcuts: bound to no panel
    const open = m.index! + m[0].indexOf("(");
    const end = closeOf(src, open);
    check(tagOf(src, m[1]) ?? "(unresolved element)", m[2], src.slice(open, end > 0 ? end : undefined), m.index!);
  }
  for (const m of src.matchAll(/([\w$.]+)\.on([a-z]+)\s*=(?!=)/g)) {
    if (/^(?:window|document|globalThis)$/.test(m[1])) continue;
    check(tagOf(src, m[1]) ?? "(unresolved element)", m[2], exprFrom(src, m.index! + m[0].length), m.index!);
  }
  return out;
}

/** Bracket depth at the end of `s` (strings skipped). */
function closeOfDepth(s: string): number {
  let depth = 0;
  for (let i = 0; i < s.length; i++) {
    const c = s[i];
    if (c === '"' || c === "'" || c === "`") { for (i++; i < s.length && s[i] !== c; i++) if (s[i] === "\\") i++; continue; }
    if (c === "{" || c === "(" || c === "[") depth++;
    else if (c === "}" || c === ")" || c === "]") depth--;
  }
  return depth;
}

// ---- what is NOT a big panel (everything else under src/app/ is, fail closed) ----

/** Modules exempt from rule 4, each with the reason. Adding one is a design decision - say why. */
const NOT_A_PANEL: Record<string, string> = {
  "src/app/chrome/map-controls.ts": "band-2 small chrome: Go-to box, zoom cluster, follow-live FAB, layers button, the retune offer (§10.6 rule 5: these ARE the small controls)",
  "src/app/centre/surface.ts": "the canvas itself (band 0/1): a bare drag pans, a wheel zooms (ui/CONTROLS.md) - the map, not a panel over it",
  "src/app/centre/nudge.ts": "the tuning-nudge cluster: small band-2 buttons, the model for rule 4",
  "src/app/decode/": "the decoder workbench is a separate view, not a panel over the map",
};

function panelFiles(dir = "src/app"): string[] {
  const out: string[] = [];
  for (const e of readdirSync(dir)) {
    const p = join(dir, e);
    if (statSync(p).isDirectory()) out.push(...panelFiles(p));
    else if (p.endsWith(".ts")) out.push(p);
  }
  return out.filter((f) => !Object.keys(NOT_A_PANEL).some((x) => f === x || (x.endsWith("/") && f.startsWith(x))));
}

// ---- the guard ----

test("P4: no device route or view jump is bound to a big-panel body or row - only to small controls", () => {
  const files = panelFiles();
  // The panels the rule names are all in scope (fail closed: a rename must not silently drop one).
  for (const f of ["src/app/chrome/sheet.ts", "src/app/chrome/focus-sheet.ts", "src/app/explore/index.ts"]) {
    assert.ok(files.includes(f), `${f} (a big panel) is guarded`);
  }
  const bad = files.flatMap((f) => bigPanelViolations(relative(".", f), readFileSync(f, "utf8")));
  assert.deepEqual(bad.map((b) => `${b.file}:${b.line} <${b.tag}> on${b.event} -> ${b.hit}`), [],
    "a big panel's row/body press moves the map or reaches a device route: put it on a small per-row <button> (docs/23 §10.6 rule 4)");
});

test("P4: the guard really parses the live panels (not vacuous): a jump injected into each real row goes red", () => {
  // The left column's inventory row, its selection row, and the Selected sheet's "found inside" row
  // all set focus on a bare click today. Swap that for a view jump in the REAL source and each one
  // must be reported, at the row's own tag - proof the scanner reads these modules' actual shape.
  const src = readFileSync("src/app/explore/index.ts", "utf8");
  const rowClicks = [...src.matchAll(/onclick: \(\) => ctx\.store\.set\(focus(?:Signal|Selection)\((?:r|s)\.id\)\)/g)];
  assert.ok(rowClicks.length >= 3, `found ${rowClicks.length} row-select clicks in explore/index.ts`);
  const injected = src.replace(/onclick: \(\) => ctx\.store\.set\(focus(Signal|Selection)\(((?:r|s))\.id\)\)/g,
    "onclick: () => host.jumpTo($2.id)");
  const bad = bigPanelViolations("src/app/explore/index.ts", injected);
  assert.equal(bad.length, rowClicks.length, JSON.stringify(bad));
  assert.ok(bad.every((b) => b.tag === "div" && b.hit === "jumpTo"), JSON.stringify(bad));
  // And the sheet's own title strip (a div): a jump on its click is caught through `addEventListener`.
  const sheet = readFileSync("src/app/chrome/sheet.ts", "utf8")
    .replace('head.addEventListener("click", () => { if (snap === "peek") ctl.set("half"); });',
      'head.addEventListener("click", () => { host.followLive(); });');
  assert.deepEqual(bigPanelViolations("sheet.ts", sheet).map((b) => [b.tag, b.hit]), [["div", "followLive"]]);
});

test("P4: every NOT_A_PANEL exemption still names a real module (a stale exemption hides nothing, but lies)", () => {
  for (const f of Object.keys(NOT_A_PANEL)) {
    assert.ok(statSync(f.replace(/\/$/, "")), `${f} exists`);
  }
});

test("P4: selecting (focus) highlights only - no focus subscriber moves the map or reaches a device", () => {
  // A row click sets `focus`; the rule would be defeated one hop away if a subscriber to `focus`
  // then jumped the view. Every `select((s) => s.focus …)` handler anywhere in src/app is checked.
  const hits: string[] = [];
  let subscribers = 0;
  for (const f of panelFiles().concat(Object.keys(NOT_A_PANEL).filter((x) => x.endsWith(".ts")))) {
    const src = readFileSync(f, "utf8");
    const defs = definitions(src);
    for (const m of src.matchAll(/\.select\(\s*\(\s*\w+\s*\)\s*=>\s*\w+\.focus\b/g)) {
      subscribers++;
      const open = m.index! + m[0].indexOf("(");
      const end = closeOf(src, open);
      const hit = MAJOR.exec(expand(src.slice(open, end), defs));
      if (hit) hits.push(`${f}:${lineOf(src, m.index!)} -> ${hit[0]}`);
    }
  }
  assert.ok(subscribers >= 1, "the Selected sheet's focus subscriber is found (not vacuous)");
  assert.deepEqual(hits, []);
});

// ---- the guard's teeth: injected violations go red, the same call on a button stays green ----

const inject = (src: string) => bigPanelViolations("injected.ts", src);

test("RED on an injected row-click view jump; GREEN when it moves onto a per-row button", () => {
  const rowJump = `
    function renderRow(r) {
      return h("div", { class: "row", tabindex: "0", onclick: () => host.jumpTo(r.f_center_hz) },
        h("div", { class: "f" }, r.f));
    }`;
  const found = inject(rowJump);
  assert.equal(found.length, 1, JSON.stringify(found));
  assert.equal(found[0].tag, "div");
  assert.equal(found[0].event, "click");
  assert.equal(found[0].hit, "jumpTo");

  const buttonJump = `
    function renderRow(r) {
      return h("div", { class: "row", tabindex: "0", onclick: () => store.set(focusSignal(r.id)) },
        h("div", { class: "f" }, r.f),
        h("button", { class: "mini go", type: "button", "aria-label": "Go to", onclick: () => host.jumpTo(r.f_center_hz) }, "Go"));
    }`;
  assert.deepEqual(inject(buttonJump), [], "a small explicit per-row button may jump the view");
});

test("RED on an injected row-click retune / device route, a keyboard twin, and a dblclick", () => {
  for (const [src, hit] of [
    [`h("li", { onclick: () => client.post("/api/control/center", { hz }) })`, "/api/control"],
    [`h("div", { class: "row", onkeydown: (e) => { if (e.key === "Enter") offerRetune(r); } })`, "offerRetune"],
    [`h("tr", { ondblclick: () => void ctx.client.open("/ws/open/listen") })`, "/ws/open"],
    [`h("div", { onclick: () => pane.followLive() })`, "followLive"],
    [`h("div", { onclick: () => view.zoomBy(0.5) })`, "zoomBy"],
    [`h("div", { onpointerdown: () => views.restoreSavedView(v) })`, "restoreSavedView"],
  ] as const) {
    const found = inject(src);
    assert.equal(found.length, 1, `${src}: ${JSON.stringify(found)}`);
    assert.equal(found[0].hit, hit, src);
    assert.deepEqual(inject(src.replace(/h\("\w+"/, 'h("button"')), [], `${src} on a <button> is allowed`);
  }
});

test("RED when the jump hides behind a same-file helper, two calls deep", () => {
  const src = `
    function select(r) { store.set(focusSignal(r.id)); reveal(r); }
    const reveal = (r) => { host.centreOn(r.f_center_hz); };
    export function row(r) { return h("div", { class: "row", onclick: () => select(r) }); }`;
  const found = inject(src);
  assert.equal(found.length, 1, JSON.stringify(found));
  assert.equal(found[0].hit, "centreOn");
});

test("RED on a listener bound to a panel body / unresolved host; GREEN on a created <button>", () => {
  const body = `
    export const mountDrawer = (el, ctx) => {
      el.addEventListener("click", (e) => { const id = e.target.dataset.id; if (id) ctx.view.flyTo(id); });
    };`;
  const found = inject(body);
  assert.equal(found.length, 1, JSON.stringify(found));
  assert.equal(found[0].tag, "(unresolved element)", "a mount host is a panel body: fail closed");
  assert.equal(found[0].hit, "flyTo");

  const onprop = `const row = h("div", { class: "row" }); row.onclick = () => retuneTo(r);`;
  assert.equal(inject(onprop).length, 1, "an `.onclick =` assignment on a row is caught too");

  const button = `
    const go = document.createElement("button");
    go.addEventListener("click", () => ctx.view.flyTo(id));`;
  assert.deepEqual(inject(button), []);
  // A row press that only selects / highlights, and a non-press event, are fine on any element.
  assert.deepEqual(inject(`h("div", { class: "row", onclick: () => store.set(focusSignal(r.id)) })`), []);
  assert.deepEqual(inject(`const s = h("section", {}); s.addEventListener("scroll", () => zoomHint());`), []);
});
