// T-824 (MAP-24): the phone-width + fade/immersive pass, at the unit tier. The browser tier
// (`ui/e2e/app-phone.e2e.mjs`) proves the layout, the fade and real touch at 400 px; this proves, with
// no browser, the wiring a later edit could quietly cut:
//  1. **One idle signal.** The HUD and the chrome CSS read `chrome-idle` on <body>; before this ticket
//     nothing set it, so the HUD's fade was dead code. The cluster's `IdleFade` is now its one writer.
//  2. **Fade never reaches what §10.2 exempts** — the sheet, Research, open menus, the retune offer, the
//     mode banner, and the honesty statements (pane rows, ring words, coverage sentence) — and a
//     focused control or a shown stream-status line holds its element solid.
//  3. **The phone breakpoint is one number**, shared by the CSS and the sheet's peek-on-Research rule.
//  4. **Nothing new reaches a route** (the spy-client rule, docs/23 §10.4): none of the touched modules
//     names an API path.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { IDLE_CLASS } from "../src/app/chrome/map-controls";
import { isPhoneWidth, PHONE_MAX_PX } from "../src/app/chrome/focus-sheet";

const src = (f: string) => readFileSync(f, "utf8");
const noComments = (s: string) => s.replace(/\/\*[\s\S]*?\*\//g, "").replace(/(^|[^:])\/\/.*$/gm, "$1");
const phoneCss = noComments(src("src/app/chrome/phone.css"));

test("one idle signal: the cluster's IdleFade writes the <body> class the HUD and the chrome CSS read", () => {
  assert.equal(IDLE_CLASS, "chrome-idle");
  const controls = noComments(src("src/app/chrome/map-controls.ts"));
  assert.match(controls, /new IdleFade\(\(idle\) => \{[\s\S]*?document\.body\?\.classList\.toggle\(IDLE_CLASS, idle\)/,
    "IdleFade's onChange must state the idle class on <body>");
  assert.match(noComments(src("src/app/centre/surface.ts")), /classList\.contains\("chrome-idle"\)/, "the HUD reads it");
  assert.match(noComments(src("src/app/centre/centre.css")), /body\.chrome-idle/, "the HUD labels read it");
  assert.match(phoneCss, /body\.chrome-idle/, "the rest of the floating chrome reads it");
  // Returns on any pointer, touch, key, wheel or focus event (§10.2).
  for (const ev of ["pointermove", "pointerdown", "keydown", "wheel", "touchstart", "focusin"]) {
    assert.match(controls, new RegExp(`"${ev}"`), `the fade does not return on ${ev}`);
  }
});

test("fade reaches the Active-outputs strip — and the pills, by being in the cluster — never what §10.2 exempts", () => {
  const rules = [...phoneCss.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map((m) => ({ sel: m[1].trim(), body: m[2] }));
  const fading = rules.filter((r) => /opacity:\s*\.35/.test(r.body));
  assert.ok(fading.length > 0, "no fade rule at all, so this proves nothing");
  const sels = fading.flatMap((r) => r.sel.split(",").map((s) => s.trim()));
  for (const s of sels) assert.match(s, /^body\.chrome-idle /, `a fade not gated on idle: ${s}`);
  // T-993: no top bar over the map to fade — its controls are in the cluster and fade with it.
  // T-997: nor the lists' chip, which is retired; the pills that replaced it are IN the cluster and
  // carry `map-fade`, so they fade by that one rule (asserted at the foot of this test).
  // T-994: the dock bar is retired; the Active-outputs strip that replaced it fades like it did.
  assert.ok(sels.some((s) => s.includes("> .out-strip")), "> .out-strip does not fade");
  assert.match(src("src/app/chrome/map-controls.ts"), /class: "map-glass map-inv map-fade"/,
    "the inventory pills must fade with the rest of the cluster");
  const NEVER = [".sheet", ".research", ".map-offer", ".map-mode", ".map-layers", ".map-pane-menu", ".sf-chrome", ".sf-note", ".sf-ring", ".sf-readout", ".side"];
  for (const s of sels) for (const n of NEVER) assert.ok(!s.includes(n), `${s} fades ${n}, which §10.2 says never fades`);
  assert.ok(sels.every((s) => /:not\(:focus-(within|visible)\)/.test(s)), "a focused control must never fade");
  assert.ok(!sels.some((s) => s.includes("> .bar")), "T-993: the retired top bar has no rule over the map");
  // The stream-status line moved into the cluster's status pill (T-993): it holds the pill solid.
  const ctlCss = noComments(src("src/app/chrome/map-controls.css"));
  assert.match(ctlCss, /\.map-ctl\.is-idle \.map-status:has\(\.conn:not\(\[hidden\]\)\) \{ opacity: 1; \}/,
    "the status pill must stay solid while it states the stream is not live");
  assert.match(ctlCss, /\.map-ctl\.is-idle \.map-fade:not\(:focus-within\) \{ opacity: \.35; \}/, "…and fades otherwise");
});

test("the phone breakpoint is one number, and isPhoneWidth reads it", () => {
  assert.match(phoneCss, new RegExp(`@media \\(max-width: ${PHONE_MAX_PX}px\\)`), "phone.css and PHONE_MAX_PX disagree");
  const asked: string[] = [];
  const win = (matches: boolean) => ({ matchMedia: (q: string) => { asked.push(q); return { matches }; } });
  assert.equal(isPhoneWidth(win(true)), true);
  assert.equal(isPhoneWidth(win(false)), false);
  assert.deepEqual(asked, [`(max-width: ${PHONE_MAX_PX}px)`, `(max-width: ${PHONE_MAX_PX}px)`]);
  assert.equal(isPhoneWidth(undefined), false, "no window (a headless test) is not a phone");
  assert.equal(isPhoneWidth({}), false);
});

test("Research opening at phone width drops the sheet to peek, without persisting it", () => {
  const fs = noComments(src("src/app/chrome/focus-sheet.ts"));
  assert.match(fs, /s\.research\?\.open === true[\s\S]*?isPhoneWidth\(\)[\s\S]*?sheet\.set\("peek", false\)/);
});

test("nothing this pass touched names a route (the spy-client rule)", () => {
  for (const f of ["src/app/chrome/phone.css", "src/app/chrome/focus-sheet.ts", "src/surface/input.ts"]) {
    assert.doesNotMatch(noComments(src(f)), /\/api\//, `${f} names an API route`);
  }
});
