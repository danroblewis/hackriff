// T-900 (user P1, docs/23 §10.6 rule 1): overlays are closeable, and Escape closes the TOPMOST one
// only. The stack is pure; the wiring is checked on the real modules' sources (every overlay goes
// through `trackOverlay`, none keeps a private Escape listener that would close several at once).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { OverlayStack, trackOverlay, wireEscape } from "../src/app/chrome/dismiss";

test("Escape closes the most recently opened overlay, one per press", () => {
  const s = new OverlayStack();
  const closed: string[] = [];
  const reg = (id: string) => ({ open: (on: boolean) => (on ? s.opened(id, () => { closed.push(id); s.closed(id); }) : s.closed(id)) });
  const sheet = reg("sheet"), side = reg("side"), layers = reg("layers");
  sheet.open(true); side.open(true); layers.open(true);
  sheet.open(true); // a re-apply (resize) keeps its place, it does not jump to the top
  assert.deepEqual(s.ids(), ["sheet", "side", "layers"]);
  assert.equal(s.escape(), true);
  assert.deepEqual(closed, ["layers"]);
  side.open(false); // closed by its own ×
  assert.equal(s.top(), "sheet");
  assert.equal(s.escape(), true);
  assert.deepEqual(closed, ["layers", "sheet"]);
  assert.equal(s.escape(), false, "nothing open: Escape is not ours");
});

test("the one window listener: only Escape, only when not already handled, prevents default", () => {
  const handlers: ((e: unknown) => void)[] = [];
  const win = { addEventListener: (_t: string, fn: (e: unknown) => void) => handlers.push(fn) } as unknown as Window;
  const s = new OverlayStack();
  wireEscape(win, s);
  wireEscape(win, s);
  assert.equal(handlers.length, 1, "installed once per window");
  const g = globalThis as Record<string, unknown>;
  const saved = g.window;
  g.window = win;
  let n = 0;
  try {
    const h = trackOverlay("x", () => { n++; s.closed("x"); }, s);
    h.open(true);
    let prevented = false;
    handlers[0]({ key: "Enter", defaultPrevented: false, preventDefault: () => { prevented = true; } });
    assert.equal(n, 0);
    handlers[0]({ key: "Escape", defaultPrevented: true, preventDefault: () => { prevented = true; } });
    assert.equal(n, 0, "a menu that already handled Escape is left alone");
    handlers[0]({ key: "Escape", defaultPrevented: false, preventDefault: () => { prevented = true; } });
    assert.equal(n, 1);
    assert.equal(prevented, true);
    assert.equal(s.top(), null);
  } finally {
    g.window = saved;
  }
});

test("every map overlay registers on the stack and has a visible dismiss; no private Escape handlers", () => {
  const src = (f: string) => readFileSync(`src/app/chrome/${f}`, "utf8");
  // T-997: the left-hand lists overlay (`side-chip.ts`) is retired — the lists are sheet content
  // and the pills that open them (`inv-pills.ts`) are chrome, not an overlay, so they register
  // nothing and dismiss through the sheet's own close.
  for (const f of ["sheet.ts", "map-controls.ts"]) {
    const s = src(f);
    assert.match(s, /trackOverlay\(/, `${f} does not register its overlay`);
    assert.doesNotMatch(s, /"Escape"/, `${f} handles Escape privately (it would close more than the topmost)`);
  }
  assert.match(src("sheet.ts"), /className = "sheet-close"/);
  const pills = src("inv-pills.ts");
  assert.doesNotMatch(pills, /trackOverlay\(|"Escape"/, "the pills are chrome, not an overlay with its own Escape");
  assert.match(src("map-controls.ts"), /class: "map-layers-close"/);
  assert.match(src("map-controls.ts"), /class: "map-offer-x"/);
  assert.match(src("map-controls.ts"), /map-pane-close"/);
  assert.match(src("map-controls.ts"), /trackOverlay\("pane-menu"/);
  // Fade is a courtesy on band-2 chrome only: no overlay panel carries the fade class.
  const ctl = src("map-controls.ts");
  assert.doesNotMatch(ctl, /class: "map-glass map-layers[^"]*map-fade/);
});
