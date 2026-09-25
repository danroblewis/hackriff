// T-896 (docs/23 §10.6 rule 3, §10.2; ADR-0023 §1): WE decide the overlay arrangement. No band-2/3
// overlay is draggable or repositionable; the user chooses only WHETHER each is shown. So:
//
//   1. no chrome dock POSITION is read from or written to localStorage / sessionStorage — only
//      visibility, the sheet's snap state and other non-positional per-viewer preferences may be,
//      and every storage key the client uses is classified here (a new key must be added, and a
//      positional one is refused by name);
//   2. no chrome element (anything under ui/src/app — band 2/3 DOM) has a drag/move handler that
//      moves it: a pointermove/mousemove/touchmove/drag handler there may resize (the sheet's
//      vertical peek/half/full snap, T-803, is a size state of a fixed-position panel) but never
//      writes left/top/right/bottom/inset/transform, and nothing there is `draggable`.
//
// A static guard over the source, because the defect it prevents is a *new* code path, which no
// behavioural test of today's panels would reach. Each detector is proven non-vacuous below by
// injecting a position write into real source and asserting it goes red.
//
// Part of T-825's (MAP-25) map-UI guard suite; T-825 folds this file in rather than duplicating it.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

// ---- the source under guard ----

function walk(dir: string): string[] {
  const out: string[] = [];
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...walk(p));
    else if (p.endsWith(".ts")) out.push(p);
  }
  return out;
}

/** Strip line and block comments so a comment mentioning "drag" or "left" is not code. Strings are
 * kept (storage keys are string literals). Good enough for this codebase's TS; not a parser. */
function stripComments(src: string): string {
  let out = "";
  let i = 0;
  let quote: string | null = null;
  while (i < src.length) {
    const c = src[i];
    if (quote) {
      out += c;
      if (c === "\\") { out += src[i + 1] ?? ""; i += 2; continue; }
      if (c === quote) quote = null;
      i++;
      continue;
    }
    if (c === '"' || c === "'" || c === "`") { quote = c; out += c; i++; continue; }
    if (c === "/" && src[i + 1] === "/") { while (i < src.length && src[i] !== "\n") i++; continue; }
    if (c === "/" && src[i + 1] === "*") {
      const end = src.indexOf("*/", i + 2);
      i = end < 0 ? src.length : end + 2;
      continue;
    }
    out += c;
    i++;
  }
  return out;
}

// ---- detector 1: positional storage ----

/** Every per-viewer storage key the client uses, and what it holds. None is a position; adding a key
 * means adding it here, with its class — and a positional class does not exist. */
const STORAGE_KEYS: Record<string, "visibility" | "snap" | "display-pref" | "session-token"> = {
  "hk-mui-show-signals": "visibility", // found-signal overlay shown/hidden (T-522)
  "hk-mui-sheet-selected": "snap", // the Selected sheet's peek/half/full (T-803)
  "hk-map-layers": "visibility", // per-pane { base style, layer visible } (T-806's layers menu)
  "hk-mui-prefs": "display-pref", // { mode, theme }
  "hk-mui-shadow-gain": "display-pref",
  "hk-surface-range-mode": "display-pref",
  "hk-ruler-mode": "display-pref", // T-1007: the time ruler's labels — seconds-ago or timestamp
  "hk-token": "session-token",
};

/** Words that name a place on screen. A storage key or a stored value built from any of them is a
 * position being persisted. */
const POSITIONAL =
  /\b(left|top|right|bottom|x|y|dx|dy|pos|position|offset\w*|dock\w*|coords?|translate\w*|transform|inset|anchor|drag\w*|moved?To|clientX|clientY|pageX|pageY|getBoundingClientRect)\b|(?:^|[-_])(pos|position|left|top|right|bottom|dock|offset|x|y)(?:$|[-_])/i;

/** The argument text of a call starting at `open` (the index of its `(`), balanced. */
function callArgs(src: string, open: number): string {
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === "(") depth++;
    else if (src[i] === ")" && --depth === 0) return src.slice(open + 1, i);
  }
  return src.slice(open + 1);
}

function storageViolations(file: string, raw: string): string[] {
  const src = stripComments(raw);
  const v: string[] = [];
  // String constants in this file, so `setItem(FOO_KEY, …)` can be resolved to its literal.
  const consts = new Map<string, string>();
  for (const m of src.matchAll(/\b(?:const|let)\s+(\w+)\s*=\s*["'`]([^"'`]*)["'`]/g)) consts.set(m[1], m[2]);
  const keyLiteral = (arg: string): string | null => {
    const a = arg.trim();
    const lit = /^["'`]([^"'`]*)["'`]$/.exec(a);
    if (lit) return lit[1];
    return consts.get(a) ?? null;
  };
  const checkKey = (key: string, where: string) => {
    if (POSITIONAL.test(key)) v.push(`${file}: ${where} uses a positional storage key "${key}"`);
    else if (!(key in STORAGE_KEYS)) v.push(`${file}: ${where} uses unclassified storage key "${key}" — add it to STORAGE_KEYS with its (non-positional) class`);
  };
  for (const m of src.matchAll(/\.(getItem|setItem|removeItem)\s*\(/g)) {
    const args = callArgs(src, m.index! + m[0].length - 1);
    const comma = args.indexOf(",");
    const keyArg = comma < 0 ? args : args.slice(0, comma);
    const valArg = comma < 0 ? "" : args.slice(comma + 1);
    const key = keyLiteral(keyArg);
    // A key passed through a parameter (the sheet's `writeSnap(storage, key, s)`) is checked where
    // the literal is declared: every `*_KEY` constant and `storageKey:` option below.
    if (key !== null) checkKey(key, m[1]);
    if (m[1] === "setItem" && POSITIONAL.test(valArg)) {
      v.push(`${file}: setItem writes a positional value \`${valArg.trim()}\``);
    }
  }
  for (const m of src.matchAll(/\b(\w*_KEY)\s*=\s*["'`]([^"'`]*)["'`]/g)) checkKey(m[2], m[1]);
  for (const m of src.matchAll(/\bstorageKey\s*:\s*([^,}\n]+)/g)) {
    const key = keyLiteral(m[1]);
    if (key !== null) checkKey(key, "storageKey");
  }
  return v;
}

// ---- detector 2: chrome that moves when dragged ----

const MOVE_EVENTS = /addEventListener\(\s*["'](pointermove|mousemove|touchmove|drag|dragstart|dragend|dragover|drop)["']\s*,\s*/g;
const POSITION_WRITE =
  /\.style\.(left|top|right|bottom|inset\w*|transform|translate|marginLeft|marginTop)\s*=|\.style\.setProperty\(\s*["'](left|top|right|bottom|inset[\w-]*|transform|translate|margin-left|margin-top)["']|\.style\.cssText\s*=|\.(moveTo|moveBy)\s*\(/;

/** The balanced `{…}` body starting at or after `from`. */
function braceBody(src: string, from: number): string {
  const open = src.indexOf("{", from);
  if (open < 0) return "";
  let depth = 0;
  for (let i = open; i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}" && --depth === 0) return src.slice(open, i + 1);
  }
  return src.slice(open);
}

/** The body of a handler passed as `arg`: an inline arrow/function, or a named one in this file. */
function handlerBody(src: string, at: number): string {
  const rest = src.slice(at);
  const named = /^(\w+)\s*[,)]/.exec(rest);
  if (!named) return braceBody(src, at);
  const n = named[1];
  const decl = new RegExp(`(?:function\\s+${n}\\s*\\(|(?:const|let|var)\\s+${n}\\s*=)`).exec(src);
  // A handler we cannot see (imported) is not proof of innocence.
  return decl ? braceBody(src, decl.index) : `<unresolved handler ${n}>`;
}

function dragMoveViolations(file: string, raw: string): string[] {
  const src = stripComments(raw);
  const v: string[] = [];
  for (const m of src.matchAll(MOVE_EVENTS)) {
    const body = handlerBody(src, m.index! + m[0].length);
    if (body.startsWith("<unresolved")) v.push(`${file}: ${m[1]} handler ${body} — cannot prove it does not move chrome`);
    else if (POSITION_WRITE.test(body)) v.push(`${file}: a ${m[1]} handler writes a position (${POSITION_WRITE.exec(body)![0]})`);
  }
  if (/\.draggable\s*=\s*true|setAttribute\(\s*["']draggable["']|draggable=["']?true/.test(src)) {
    v.push(`${file}: makes a chrome element draggable`);
  }
  return v;
}

// ---- the guard ----

const allSrc = walk("src");
// Band 2/3 chrome is the app shell's DOM. `src/surface` is band 0/1 (the canvas and content-anchored
// pins, laid out per frame by design) and its pointer handlers pan the VIEW, not an overlay.
const chromeSrc = allSrc.filter((f) => f.startsWith(join("src", "app")));

test("P3: no chrome position is read from or written to localStorage/sessionStorage", () => {
  const v = allSrc.flatMap((f) => storageViolations(f, readFileSync(f, "utf8")));
  assert.deepEqual(v, []);
});

test("P3: every storage key the client declares is classified, and none is positional", () => {
  const declared = new Set<string>();
  for (const f of allSrc) {
    for (const m of stripComments(readFileSync(f, "utf8")).matchAll(/\b\w*_KEY\s*=\s*["'`](hk-[^"'`]*)["'`]/g)) declared.add(m[1]);
  }
  for (const k of declared) assert.ok(k in STORAGE_KEYS, `unclassified storage key ${k}`);
  for (const k of Object.keys(STORAGE_KEYS)) assert.doesNotMatch(k, POSITIONAL);
});

test("P3: no band-2/3 chrome element has a drag handler that moves it", () => {
  assert.ok(chromeSrc.length > 10, "the chrome source was found");
  const v = chromeSrc.flatMap((f) => dragMoveViolations(f, readFileSync(f, "utf8")));
  assert.deepEqual(v, []);
});

test("P3: the sheet's drag is a size (height) change of a fixed-position panel, and that is allowed", () => {
  const sheet = readFileSync("src/app/chrome/sheet.ts", "utf8");
  assert.match(sheet, /addEventListener\("pointermove"/, "the sheet still has a drag (else this guard is testing nothing)");
  assert.match(sheet, /host\.style\.height =/);
  assert.deepEqual(dragMoveViolations("sheet.ts", sheet), []);
});

// ---- non-vacuity: each detector goes red on a deliberately injected position write ----

test("red: persisting a dock position under a new key", () => {
  const src = readFileSync("src/app/chrome/sheet.ts", "utf8") +
    `\nexport function saveDock(el: HTMLElement) { localStorage.setItem("hk-mui-sheet-pos", JSON.stringify({ x: el.offsetLeft, y: el.offsetTop })); }\n`;
  const v = storageViolations("sheet.ts", src);
  assert.ok(v.some((s) => s.includes("positional storage key")), v.join("\n"));
  assert.ok(v.some((s) => s.includes("positional value")), v.join("\n"));
});

test("red: smuggling a position into an existing, allowed key", () => {
  const src = readFileSync("src/app/shell.ts", "utf8").replace(
    "JSON.stringify({ mode, theme } satisfies Prefs)",
    "JSON.stringify({ mode, theme, left: panel.style.left })",
  );
  assert.notEqual(src, readFileSync("src/app/shell.ts", "utf8"), "the injection point still exists");
  assert.ok(storageViolations("shell.ts", src).some((s) => s.includes("positional value")));
});

test("red: reading a position back from storage", () => {
  const src = `const DOCK_KEY = "hk-mui-layers-dock";\nconst at = localStorage.getItem(DOCK_KEY);\n`;
  assert.ok(storageViolations("x.ts", src).some((s) => s.includes("positional storage key")));
});

test("red: an unclassified key (a new persisted thing must be declared non-positional)", () => {
  const src = `const LAYOUT_KEY = "hk-mui-layout";\nlocalStorage.setItem(LAYOUT_KEY, s);\n`;
  assert.ok(storageViolations("x.ts", src).some((s) => s.includes("unclassified")));
});

test("red: the sheet's drag handler moving the sheet instead of sizing it", () => {
  const real = readFileSync("src/app/chrome/sheet.ts", "utf8");
  const src = real.replace(
    "host.style.height = `${Math.max(hs.peek, Math.min(hs.full, drag.h0 - dy))}px`;",
    "host.style.height = `${Math.max(hs.peek, Math.min(hs.full, drag.h0 - dy))}px`;\n    host.style.left = `${ev.clientX}px`;",
  );
  assert.notEqual(src, real, "the injection point still exists");
  assert.ok(dragMoveViolations("sheet.ts", src).some((s) => s.includes("writes a position")));
});

test("red: a named move handler that translates a panel", () => {
  const src = `function onMove(ev: PointerEvent) { panel.style.transform = \`translate(\${ev.clientX}px, 0)\`; }\npanel.addEventListener("pointermove", onMove);\n`;
  assert.ok(dragMoveViolations("x.ts", src).some((s) => s.includes("writes a position")));
});

test("red: a draggable chrome element", () => {
  assert.ok(dragMoveViolations("x.ts", `layers.draggable = true;\n`).some((s) => s.includes("draggable")));
  assert.ok(dragMoveViolations("x.ts", `el.setAttribute("draggable", "true");\n`).some((s) => s.includes("draggable")));
});

test("comments are not code (a comment mentioning a drag or a position is not a violation)", () => {
  const src = `// panel.style.left = x on pointermove\n/* localStorage.setItem("hk-pos", x) */\nconst a = 1;\n`;
  assert.deepEqual(storageViolations("x.ts", src), []);
  assert.deepEqual(dragMoveViolations("x.ts", src), []);
});
