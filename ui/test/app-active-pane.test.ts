// T-1000 (MMAP split view): the ACTIVE pane, made visible (docs/23 §10.7).
//
// Before this ticket Go-to, zoom, the layers menu, the follow-live FAB and the viewport menu all
// acted on `SurfacePreview.activePane` — the pane last pressed or wheeled — and nothing on screen
// said which pane that was; a right-click did not even change it. The claims:
//
//  1. **Naming is by layout position, and says nothing with one pane** — "pane 2 of 2", never the
//     internal id, and `null` (no outline, no badge) when there is nothing to disambiguate.
//  2. **Every writer of the active pane notifies, in the same call** — the accessor refuses an id
//     that is not a pane, fires once per real change, and never for a no-op.
//  3. **A right-click (and the context-menu event) sets the active pane**, and — like every other
//     way of setting it — moves no view: the spy preview's call list stays empty.
//  4. **The pane keys** — `[`/`]` step, `1`-`9` pick, `L` toggles Live — are bare keys only: never
//     while typing, never with a modifier (Cmd+L is the address bar), never on auto-repeat.
//  5. **The outline is placed from the pane's own rectangle**, drawing-buffer GL px → CSS px.
//  6. **The wiring**: the surface mount places the outline in the render frame's `dom` hook and on
//     every active-pane change, and the chrome names the pane on each per-pane control.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { activePaneName, isTypingTarget, outlineBox, paneKeyIntent, stepPane } from "../src/app/centre/active-pane";
import { attachSurfaceInput } from "../src/surface/input";
import { SurfacePreview } from "../src/surface/preview";

// ---------------------------------------------------------------------------
// 1. naming
// ---------------------------------------------------------------------------

test("the active pane is named by its layout position, and not at all with one pane", () => {
  assert.equal(activePaneName(["pane1"], "pane1"), null, "one pane: nothing to disambiguate");
  assert.deepEqual(activePaneName(["pane1", "pane3"], "pane3"), { n: 2, count: 2, label: "pane 2 of 2" },
    "named by POSITION: pane3 is the second pane on screen after pane2 closed");
  assert.deepEqual(activePaneName(["a", "b", "c"], "a"), { n: 1, count: 3, label: "pane 1 of 3" });
  assert.equal(activePaneName(["a", "b"], "gone"), null, "an id that is not a pane names nothing");
  assert.equal(activePaneName(["a", "b"], null), null);
});

test("stepping through the panes wraps both ways, and has nowhere to go with one", () => {
  const ids = ["a", "b", "c"];
  assert.equal(stepPane(ids, "a", 1), "b");
  assert.equal(stepPane(ids, "c", 1), "a");
  assert.equal(stepPane(ids, "a", -1), "c");
  assert.equal(stepPane(["a"], "a", 1), null);
});

// ---------------------------------------------------------------------------
// 2. the accessor notifies every change, once, and refuses a non-pane
// ---------------------------------------------------------------------------

/** A `SurfacePreview` without a canvas: only the state the accessor reads. `new` would need WebGL2;
 * the accessor itself is plain JS over `active`, `activeListeners` and `view.panes.has`. */
function bareAccessor(ids: string[]) {
  const p = Object.create(SurfacePreview.prototype) as SurfacePreview;
  Object.assign(p as unknown as Record<string, unknown>, {
    active: ids[0], activeListeners: new Set(), view: { panes: { has: (id: string) => ids.includes(id) } },
  });
  return p;
}

test("setting the active pane notifies its listeners once per real change, and refuses a non-pane", () => {
  const p = bareAccessor(["pane1", "pane2"]);
  const seen: string[] = [];
  const off = p.onActiveChange((id) => seen.push(id));
  p.activePane = "pane2";
  p.activePane = "pane2"; // no-op: already active
  p.activePane = "nope"; // not a pane
  assert.equal(p.activePane, "pane2");
  p.activePane = "pane1";
  assert.deepEqual(seen, ["pane2", "pane1"]);
  off();
  p.activePane = "pane2";
  assert.deepEqual(seen, ["pane2", "pane1"], "a disposed listener is not told");
});

// ---------------------------------------------------------------------------
// 3. right-click sets the active pane, and moves nothing
// ---------------------------------------------------------------------------

const W = 1000, H = 600;

/** Two side-by-side panes (x < 500 → "L", else "R") above a map strip (GL y < 50). */
function harness(opts: Parameters<typeof attachSurfaceInput>[2] = {}) {
  const listeners = new Map<string, (e: unknown) => void>();
  const canvas = {
    width: W, height: H,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: W, height: H }),
    setPointerCapture: () => {},
    addEventListener: (t: string, f: (e: unknown) => void) => listeners.set(t, f),
    removeEventListener: (t: string) => listeners.delete(t),
  };
  const calls: string[] = [];
  const preview = {
    activePane: "L",
    onMap: (p: { y: number }) => p.y < 50,
    paneAt: (p: { x: number }) => (p.x < 500 ? "L" : "R"),
    drag: () => calls.push("drag"), dragMap: () => calls.push("dragMap"),
    wheel: () => calls.push("wheel"), wheelMap: () => calls.push("wheelMap"),
    goToOnMap: () => calls.push("goToOnMap"),
    endDrag: () => calls.push("endDrag"), endDragMap: () => calls.push("endDragMap"),
  };
  attachSurfaceInput(canvas as unknown as HTMLCanvasElement, preview as unknown as SurfacePreview, opts);
  const fire = (type: string, e: Record<string, unknown>) => listeners.get(type)?.(e);
  return { calls, fire, preview };
}

test("a RIGHT press on a pane makes it active, and starts no stroke", () => {
  const h = harness();
  h.fire("pointerdown", { button: 2, pointerType: "mouse", clientX: 800, clientY: 300, pointerId: 1 });
  assert.equal(h.preview.activePane, "R", "a right-click on pane R did not make it active");
  h.fire("pointermove", { buttons: 2, clientX: 700, clientY: 250 });
  h.fire("pointerup", { clientX: 700, clientY: 250, pointerId: 1 });
  assert.deepEqual(h.calls, [], "a right press panned, zoomed or settled something");
});

test("the context-menu request names the pane too, and still opens the menu at the point", () => {
  const menus: unknown[] = [];
  const h = harness({ onContext: (p) => menus.push(p) });
  let prevented = false;
  h.fire("contextmenu", { clientX: 800, clientY: 300, preventDefault: () => { prevented = true; } });
  assert.equal(h.preview.activePane, "R");
  assert.ok(prevented);
  assert.deepEqual(menus, [{ x: 800, y: H - 300 }]);
  // …even with no host menu at all (the `/surface.html` preview passes none).
  const bare = harness();
  bare.fire("contextmenu", { clientX: 800, clientY: 300, preventDefault: () => {} });
  assert.equal(bare.preview.activePane, "R");
  assert.deepEqual(h.calls, []);
});

test("a right press on the map strip changes no pane", () => {
  const h = harness();
  h.fire("pointerdown", { button: 2, pointerType: "mouse", clientX: 800, clientY: H - 10, pointerId: 1 });
  h.fire("contextmenu", { clientX: 800, clientY: H - 10, preventDefault: () => {} });
  assert.equal(h.preview.activePane, "L");
});

test("a plain press still sets the active pane (unchanged)", () => {
  const h = harness();
  h.fire("pointerdown", { button: 0, pointerType: "mouse", clientX: 800, clientY: 300, pointerId: 1 });
  assert.equal(h.preview.activePane, "R");
});

// ---------------------------------------------------------------------------
// 4. the keys
// ---------------------------------------------------------------------------

test("the pane keys: ] and [ step, 1-9 pick, L toggles Live", () => {
  assert.deepEqual(paneKeyIntent({ key: "]" }), { kind: "step", step: 1 });
  assert.deepEqual(paneKeyIntent({ key: "[" }), { kind: "step", step: -1 });
  assert.deepEqual(paneKeyIntent({ key: "2" }), { kind: "index", n: 2 });
  assert.deepEqual(paneKeyIntent({ key: "l" }), { kind: "live" });
  assert.deepEqual(paneKeyIntent({ key: "L" }), { kind: "live" });
  assert.equal(paneKeyIntent({ key: "0" }), null);
  assert.equal(paneKeyIntent({ key: "x" }), null);
  assert.equal(paneKeyIntent({ key: "Escape" }), null, "Escape stays the tool mode's and the overlay stack's");
});

test("a pane key is never a command while typing, with a modifier, or on auto-repeat", () => {
  const text = { tagName: "INPUT", type: "text" };
  assert.equal(paneKeyIntent({ key: "l", target: text }), null, "typing an 'l' into Go-to toggled Live");
  assert.equal(paneKeyIntent({ key: "2", target: { tagName: "INPUT" } }), null, "an untyped input is text");
  assert.equal(paneKeyIntent({ key: "]", target: { tagName: "TEXTAREA" } }), null);
  assert.equal(paneKeyIntent({ key: "1", target: { tagName: "DIV", isContentEditable: true } }), null);
  assert.equal(paneKeyIntent({ key: "l", metaKey: true }), null, "Cmd+L is the browser's");
  assert.equal(paneKeyIntent({ key: "1", ctrlKey: true }), null);
  assert.equal(paneKeyIntent({ key: "]", altKey: true }), null);
  assert.equal(paneKeyIntent({ key: "]", repeat: true }), null);
  assert.equal(paneKeyIntent({ key: "]", defaultPrevented: true }), null);
  // A focused checkbox or button is not a text field: the key is still a command there.
  assert.equal(isTypingTarget({ tagName: "INPUT", type: "checkbox" }), false);
  assert.deepEqual(paneKeyIntent({ key: "]", target: { tagName: "BUTTON" } }), { kind: "step", step: 1 });
});

// ---------------------------------------------------------------------------
// 5. the outline's geometry
// ---------------------------------------------------------------------------

test("the outline is the pane's own rectangle, GL device px to CSS px from the top-left", () => {
  // A 1600 x 1000 device-px canvas at dpr 2; the right pane of a column split, 60 px above the bottom.
  assert.deepEqual(outlineBox({ x: 802, y: 60, w: 796, h: 900 }, 1000, 2), { left: 401, top: 20, width: 398, height: 450 });
  assert.deepEqual(outlineBox({ x: 0, y: 0, w: 10, h: 10 }, 100, 1), { left: 0, top: 90, width: 10, height: 10 });
});

// ---------------------------------------------------------------------------
// 6. the wiring, read from the source the page runs
// ---------------------------------------------------------------------------

test("the surface places the outline per frame and on every change; the chrome names the pane", () => {
  const host = readFileSync("src/app/centre/surface.ts", "utf8");
  // T-1001 added the panes' own Live buttons to the same hook; the outline's claim is unchanged.
  const domAt = host.indexOf("dom: (panes, edge, hPx, dpr)");
  const domHook = host.slice(domAt, host.indexOf("});", domAt));
  assert.ok(/pinsFrame\(panes, edge, hPx, dpr\);/.test(domHook) && /placeActive\(panes, hPx, dpr\);/.test(domHook),
    "the outline must be placed in the render frame's dom hook, from the frame's own pane rectangles");
  assert.match(host, /pv\.onActiveChange\(activeChanged\)/, "the chrome must hear every active-pane change");
  const changed = host.slice(host.indexOf("const activeChanged"), host.indexOf("pv.onActiveChange(activeChanged)"));
  for (const call of ["placeActive(", "controls.syncActive()", "syncLayerControls()", "renderFollow()"]) {
    assert.ok(changed.includes(call), `an active-pane change does not call ${call}`);
  }
  assert.match(host, /activeName: \(\) => activePaneName\(/, "the chrome and the outline must share one name");
  // T-1001: the FAB retired, so `L` presses the ACTIVE pane's own Live button — the very element
  // the user clicks, so the key and the button still cannot do different things.
  assert.match(host, /liveButtons\?\.buttonFor\(pv\.activePane\)\?\.click\(\)/, "L must press the active pane's own Live button");

  const chrome = readFileSync("src/app/chrome/map-controls.ts", "utf8");
  const sync = chrome.slice(chrome.indexOf("const syncActive"), chrome.indexOf('goto.addEventListener("submit"'));
  assert.ok(sync.length > 200 && sync.length < 2000, "the syncActive slice is not the function");
  // T-1001: `fab.` left the list with the FAB — a per-pane Live button names its own pane.
  for (const named of ["gotoPane", "layersBtn", "zoom.", "paneHead"]) {
    assert.ok(sync.includes(named), `syncActive does not name the pane on ${named}`);
  }
  // Global chrome (docs/23 §10.7): the colour scale and the outputs are never named per pane.
  assert.ok(!/scale|record/i.test(sync), "syncActive names a GLOBAL control (colour scale / Record) as per-pane");
});
