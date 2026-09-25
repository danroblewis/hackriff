// **T-458: the surface has two pointer gestures, and neither can become the other.**
//
// T-456 made drag = pan, which is what the user asked for, and the cost was the gesture
// region-select used to own. This file holds what the replacement has to be true of. Three of the
// four claims are about a *fault that must not pass*, not about the happy path — because the way
// this milestone has repeatedly gone wrong is a sound proof of an adjacent proposition.
//
//  1. **A press means one thing, decided at the press.** Shift held → mark out a region; otherwise
//     → pan. The decision is latched, so releasing shift mid-stroke cannot turn the region you are
//     marking out into a pan of the view you are marking it out on.
//  2. **A region stroke does not pan.** Not "pans a little": the viewport must not move at all
//     under a rectangle the user is drawing on it, or the rectangle describes a window that has
//     already gone. This is also what keeps T-444 true — a region stroke is not a pan, so a pending
//     retune offer is neither invalidated nor created by one.
//  3. **A TAP IS NEVER A REGION, and travel is a DISTANCE.** T-407 found two ways a finger could
//     retune the radio: a threshold that made a fat-fingered tap a drag, and travel measured as
//     `clientX + clientY`, so a stroke *across* a bar counted as travel *along* it. Both faults are
//     written out below and required to fail: a (+8, −8) stroke is an ordinary down-left rectangle
//     whose components CANCEL under a sum, so `dx + dy` silently refuses to commit it.
//  4. **Nothing here reaches anything.** `attachSurfaceInput` is handed a spy preview and a spy
//     `fetch`; the whole vocabulary — pan, wheel, region, tap — produces an empty call list, which
//     is T-340's control in this file's terms.
import { test } from "node:test";
import assert from "node:assert/strict";
import { attachSurfaceInput, DRAG_PX, type SurfaceRegion } from "../src/surface/input";
import { dragIntent, HOLD_TO_MARK_MS, pinchZoom, touchIntent } from "../src/surface/preview";
import type { SurfacePreview } from "../src/surface/preview";

// ---------------------------------------------------------------------------
// A canvas and a preview, both spies. Nothing real is needed: this file is about which callback
// runs for which event, and every one of `preview`'s methods is arithmetic T-442 already proved
// reaches nothing outside itself.
// ---------------------------------------------------------------------------

const W = 1000, H = 600;

interface Call { fn: string; args: number[] }

function harness(opts: Parameters<typeof attachSurfaceInput>[2] = {}) {
  const listeners = new Map<string, (e: unknown) => void>();
  const canvas = {
    width: W, height: H,
    getBoundingClientRect: () => ({ left: 0, top: 0, width: W, height: H }),
    setPointerCapture: () => {},
    addEventListener: (t: string, f: (e: unknown) => void) => listeners.set(t, f),
    removeEventListener: (t: string) => listeners.delete(t),
  };
  const calls: Call[] = [];
  // T-486's commit point, kept in a list of its OWN rather than in `calls`. Ending a gesture is not
  // another pan — it moves nothing — and folding it into the motion list would make every existing
  // assertion about *what the view was asked to do* read as though a release moved the viewport.
  const settles: string[] = [];
  const preview = {
    activePane: "p0",
    onMap: (p: { y: number }) => p.y < 50, // the map strip, in GL coords (origin bottom-left)
    paneAt: () => "p0",
    drag: (id: string, dx: number, dy: number) => calls.push({ fn: "drag", args: [dx, dy] }),
    dragMap: (dx: number, dy: number) => calls.push({ fn: "dragMap", args: [dx, dy] }),
    wheel: (_id: string, _p: unknown, factor: number) => calls.push({ fn: "wheel", args: [factor] }),
    wheelMap: (_p: unknown, factor: number) => calls.push({ fn: "wheelMap", args: [factor] }),
    goToOnMap: () => calls.push({ fn: "goToOnMap", args: [] }),
    endDrag: (id: string) => settles.push(id),
    endDragMap: () => settles.push("map"),
  };
  const dispose = attachSurfaceInput(
    canvas as unknown as HTMLCanvasElement, preview as unknown as SurfacePreview, opts,
  );
  const fire = (type: string, e: Record<string, unknown>) => listeners.get(type)?.(e);
  return { calls, settles, fire, dispose, preview };
}

/** A press–move(s)–release, with the modifiers named once at the press unless `shiftDuring` says
 * otherwise. `clientY` grows downward, as a browser's does. */
function stroke(
  h: ReturnType<typeof harness>,
  from: { x: number; y: number }, steps: readonly { x: number; y: number }[],
  { shift = false, shiftDuring = shift, button = 0 }: { shift?: boolean; shiftDuring?: boolean; button?: number } = {},
) {
  h.fire("pointerdown", { button, clientX: from.x, clientY: from.y, shiftKey: shift, pointerId: 1 });
  for (const s of steps) h.fire("pointermove", { buttons: 1, clientX: s.x, clientY: s.y, shiftKey: shiftDuring });
  const last = steps[steps.length - 1] ?? from;
  h.fire("pointerup", { clientX: last.x, clientY: last.y, shiftKey: shiftDuring, pointerId: 1 });
}

/** A point well inside a pane (GL y above the map strip). `clientY` 300 → GL y 300. */
const MID = { x: 400, y: 300 };

// ---------------------------------------------------------------------------
// 1. Two gestures, and which one is decided at the press
// ---------------------------------------------------------------------------

test("a plain drag pans and marks out nothing; a shift-drag marks out a region and pans NOTHING", () => {
  const plain: SurfaceRegion[] = [];
  const h1 = harness({ onRegion: (r) => plain.push(r) });
  stroke(h1, MID, [{ x: 460, y: 340 }, { x: 520, y: 380 }]);
  assert.deepEqual(h1.calls.map((c) => c.fn), ["drag", "drag"], "a plain drag pans");
  assert.deepEqual(plain, [], "…and commits no region");

  const got: SurfaceRegion[] = [];
  const h2 = harness({ onRegion: (r) => got.push(r) });
  stroke(h2, MID, [{ x: 460, y: 340 }, { x: 520, y: 380 }], { shift: true });
  assert.deepEqual(h2.calls, [],
    "a region stroke moved the viewport: the rectangle would then describe a window that has moved under it");
  assert.equal(got.length, 1, "a shift-drag commits exactly one region");
  // The corners are the press and the RELEASE, in drawing-buffer coordinates (GL y runs up).
  assert.deepEqual(got[0].a, { x: 400, y: H - 300 });
  assert.deepEqual(got[0].b, { x: 520, y: H - 380 });
  assert.equal(got[0].pane, "p0");
});

test("the meaning is LATCHED at the press: releasing shift mid-stroke does not turn a region into a pan", () => {
  const got: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => got.push(r) });
  // Shift down at the press, up by the time the moves arrive — which is what a hand does when it
  // lets go early. A modifier re-read per move would pan the view out from under the rectangle.
  stroke(h, MID, [{ x: 460, y: 340 }, { x: 520, y: 380 }], { shift: true, shiftDuring: false });
  assert.deepEqual(h.calls, [], "the stroke panned: the modifier is being re-read per move");
  assert.equal(got.length, 1, "…and the region it began as must still be the region it commits");

  // And the converse: shift pressed DURING a pan does not start a region halfway through.
  const started: (SurfaceRegion | null)[] = [];
  const h2 = harness({ onRegion: () => {}, onRegionDrag: (r) => started.push(r) });
  stroke(h2, MID, [{ x: 460, y: 340 }, { x: 520, y: 380 }], { shift: false, shiftDuring: true });
  assert.deepEqual(h2.calls.map((c) => c.fn), ["drag", "drag"], "a pan that began as a pan stays one");
  assert.deepEqual(started, [], "no region stroke may begin mid-pan");
});

test("the modifier's MEANING is preview.ts's, not this file's: `dragIntent` is the only reader", () => {
  // The architectural half of claim 1, and the reason `input.ts` can be grepped for modifier bits
  // by `surface-cutover.test.ts` and `surface-preview.test.ts` and come up empty. Asserted on the
  // function rather than only on the source, so the rule has a behavioural anchor too.
  assert.equal(dragIntent({ shiftKey: true }), "region");
  assert.equal(dragIntent({}), "pan");
  assert.equal(dragIntent({ altKey: true }), "pan", "alt is the TIME axis on a wheel and binds nothing on a drag");
  assert.equal(dragIntent({ ctrlKey: true }), "pan", "ctrl+click is macOS's secondary click — it must not mark out a region");
  assert.equal(dragIntent({ metaKey: true }), "pan");
});

// ---------------------------------------------------------------------------
// 2. A tap is never a region, and travel is a distance (T-407)
// ---------------------------------------------------------------------------

test("a shift-TAP commits nothing, and does not focus either", () => {
  const got: SurfaceRegion[] = [];
  const clicks: unknown[] = [];
  const h = harness({ onRegion: (r) => got.push(r), onClick: (p) => clicks.push(p) });
  // A fat-fingered tap: real movement, under the threshold. T-407's first defect was a threshold
  // low enough that this became a drag; here, below it, a stroke is nothing at all.
  stroke(h, MID, [{ x: 402, y: 302 }, { x: 403, y: 301 }], { shift: true });
  assert.deepEqual(got, [], "a tap must never mark out a region");
  assert.deepEqual(clicks, [], "…nor fall through to the click that focuses a row");
  assert.deepEqual(h.calls, [], "…nor pan");

  // A plain tap still focuses: the click path is untouched, which is the distinction `DRAG_PX`'s
  // comment draws and this asserts rather than assumes.
  const plain: unknown[] = [];
  const h2 = harness({ onRegion: () => {}, onClick: (p) => plain.push(p) });
  stroke(h2, MID, [{ x: 402, y: 302 }]);
  assert.equal(plain.length, 1, "a plain tap still focuses whatever is under it");
});

test("TRAVEL IS A DISTANCE, NOT A SUM: a down-left stroke whose components cancel still commits", () => {
  // THE FAULT THIS EXISTS TO CATCH, written out so it is checkable rather than asserted:
  //
  //     const far = (e.clientX - d.x0) + (e.clientY - d.y0) >= DRAG_PX;   // T-407's `clientX + clientY`
  //
  // A stroke of (+dx, −dx) is an ordinary rectangle marked out down-left (or up-right) — the most
  // natural way to drag from a signal's top-right corner. Under the sum its components cancel to
  // exactly 0, which is < DRAG_PX, so the region silently fails to commit and the user is left
  // dragging a box that never appears. Under `Math.hypot` it is 11.3 px, comfortably over.
  const dx = 8;
  assert.ok(dx + -dx < DRAG_PX, "the fault's own measure would refuse this stroke");
  assert.ok(Math.hypot(dx, -dx) >= DRAG_PX, "…and the correct one accepts it");

  const got: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => got.push(r) });
  stroke(h, MID, [{ x: MID.x + dx, y: MID.y - dx }], { shift: true });
  assert.equal(got.length, 1, "a stroke whose x and y travel cancel under a SUM must still commit");

  // The mirror case, for the same reason in the other direction.
  const up: SurfaceRegion[] = [];
  const h2 = harness({ onRegion: (r) => up.push(r) });
  stroke(h2, MID, [{ x: MID.x - dx, y: MID.y + dx }], { shift: true });
  assert.equal(up.length, 1);
});

test("the gate is the NET displacement, not the path length: a stroke that wanders back is a tap", () => {
  // The other half of "a tap is never a region", and the reason the region gate is not `d.travel`.
  // `travel` accumulates every move, so a hand that jitters out 40 px and comes back to within a
  // pixel of where it started passes a path-length test while enclosing nothing.
  const got: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => got.push(r) });
  stroke(h, MID, [{ x: 440, y: 300 }, { x: 400, y: 340 }, { x: 401, y: 301 }], { shift: true });
  assert.deepEqual(got, [], "a stroke that returned to its origin marked out nothing");
});

test("a rectangle flat on either axis is a line, and commits nothing", () => {
  for (const end of [{ x: 600, y: MID.y }, { x: MID.x, y: 500 }]) {
    const got: SurfaceRegion[] = [];
    const h = harness({ onRegion: (r) => got.push(r) });
    stroke(h, MID, [end], { shift: true });
    assert.deepEqual(got, [], `a stroke to ${JSON.stringify(end)} has no extent on one axis`);
  }
});

// ---------------------------------------------------------------------------
// 3. The pending stroke is reported live, and retracted rather than left behind
// ---------------------------------------------------------------------------

test("the stroke is reported on every move and CLEARED on release, on cancel, and on a tap", () => {
  const seen: (SurfaceRegion | null)[] = [];
  const h = harness({ onRegion: () => {}, onRegionDrag: (r) => seen.push(r) });
  stroke(h, MID, [{ x: 460, y: 340 }, { x: 520, y: 380 }], { shift: true });
  // press, two moves, then the retraction — a rubber band that lagged the pointer by a poll would
  // be T-388's box-jump wearing a different hat.
  assert.equal(seen.length, 4);
  assert.deepEqual(seen[0]!.a, seen[0]!.b, "at the press the rectangle is a point");
  assert.deepEqual(seen[2]!.b, { x: 520, y: H - 380 });
  assert.equal(seen[3], null, "the band must be retracted on release");

  // A cancelled stroke (pointer lost, a system gesture taking over) retracts too: a band left on
  // screen with no pointer behind it is a claim about a gesture that is not happening.
  const c: (SurfaceRegion | null)[] = [];
  const h2 = harness({ onRegion: () => {}, onRegionDrag: (r) => c.push(r) });
  h2.fire("pointerdown", { button: 0, clientX: MID.x, clientY: MID.y, shiftKey: true, pointerId: 1 });
  h2.fire("pointermove", { buttons: 1, clientX: 500, clientY: 400, shiftKey: true });
  h2.fire("pointercancel", {});
  assert.equal(c[c.length - 1], null, "a cancelled stroke must retract its band");

  // …and so does one that turned out to be a tap.
  const t: (SurfaceRegion | null)[] = [];
  const h3 = harness({ onRegion: () => {}, onRegionDrag: (r) => t.push(r) });
  stroke(h3, MID, [{ x: 402, y: 301 }], { shift: true });
  assert.equal(t[t.length - 1], null);
});

test("with no host interested, a shift-drag is a pan: the gesture is not reserved by a surface that cannot use it", () => {
  // `/surface.html` mounts the same handler with no `onRegion` (it has no selections to make). A
  // modifier that silently disabled panning there would be a dead zone the page could not explain.
  const h = harness({});
  stroke(h, MID, [{ x: 460, y: 340 }], { shift: true });
  assert.deepEqual(h.calls.map((c) => c.fn), ["drag"]);
});

test("a shift-drag on the MAP strip pans the map: the map has no time axis to mark out", () => {
  const got: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => got.push(r) });
  // GL y < 50 is the map strip, i.e. clientY > H - 50.
  stroke(h, { x: 400, y: 580 }, [{ x: 500, y: 570 }], { shift: true });
  assert.deepEqual(h.calls.map((c) => c.fn), ["dragMap"]);
  assert.deepEqual(got, [], "the map is a frequency ruler, not a surface to select on");
});

// ---------------------------------------------------------------------------
// 4. T-340's control, in this file's terms
// ---------------------------------------------------------------------------

test("T-340's control over the WHOLE gesture vocabulary, region stroke included: NO call reaches the network", () => {
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  const net: unknown[] = [];
  g.fetch = (...args: unknown[]) => { net.push(args); return Promise.reject(new Error("a gesture must not reach the network")); };
  const regions: SurfaceRegion[] = [];
  try {
    const h = harness({ onRegion: (r) => regions.push(r), onRegionDrag: () => {}, onClick: () => {}, onHover: () => {} });
    stroke(h, MID, [{ x: 700, y: 500 }]);                      // pan
    stroke(h, MID, [{ x: 700, y: 500 }], { shift: true });      // region
    stroke(h, MID, [{ x: 401, y: 301 }]);                       // tap
    stroke(h, { x: 400, y: 580 }, [{ x: 900, y: 575 }]);        // map pan
    for (const mods of [{}, { shiftKey: true }, { altKey: true }, { ctrlKey: true }, { metaKey: true }]) {
      h.fire("wheel", { preventDefault: () => {}, clientX: 400, clientY: 300, deltaY: -120, deltaX: 0, deltaMode: 0, ...mods });
    }
    h.fire("dblclick", { clientX: 400, clientY: 580 });
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(net, [], "a gesture reached the network: no pointer stream may command anything");
  // The control that stops "it never commits" from passing this.
  assert.equal(regions.length, 1, "no region was produced at all, so the run proves nothing");
});

// ---------------------------------------------------------------------------
// 5. T-526: Ctrl+Shift+wheel is the shadow-brightness gesture, and it is not a view gesture
// ---------------------------------------------------------------------------

test("a Ctrl+Shift wheel notch calls onShadowGain and reaches NEITHER wheel nor wheelMap: it never moves the pane's view", () => {
  const notches: number[] = [];
  const h = harness({ onShadowGain: (n) => notches.push(n) });
  h.fire("wheel", { preventDefault: () => {}, clientX: 400, clientY: 300, deltaY: -120, deltaX: 0, deltaMode: 0, ctrlKey: true, shiftKey: true });
  assert.deepEqual(h.calls, [], "ctrl+shift reached preview.wheel/wheelMap — it must stay a display-only gesture");
  assert.deepEqual(notches, [1]);
});

test("without a host-supplied onShadowGain, Ctrl+Shift+wheel falls through to the ordinary zoom", () => {
  const h = harness({});
  h.fire("wheel", { preventDefault: () => {}, clientX: 400, clientY: 300, deltaY: -120, deltaX: 0, deltaMode: 0, ctrlKey: true, shiftKey: true });
  assert.deepEqual(h.calls.map((c) => c.fn), ["wheel"], "with no shadow-gain host, the combo must still do SOMETHING sensible, not silently eat the event");
});

// ---------------------------------------------------------------------------
// 6. T-822 / MAP-22: measurement mode is a TOGGLE, not a modifier — checked first, no shift needed
// ---------------------------------------------------------------------------

test("measureMode on: an UNMODIFIED drag marks out a measurement and pans NOTHING", () => {
  const got: SurfaceRegion[] = [];
  const h = harness({ measureMode: true, onMeasure: (r) => got.push(r) });
  stroke(h, MID, [{ x: 460, y: 340 }, { x: 520, y: 380 }]);
  assert.deepEqual(h.calls, [], "a measurement stroke moved the viewport: the readout would then describe a window that moved under it");
  assert.equal(got.length, 1);
  assert.deepEqual(got[0].a, { x: 400, y: H - 300 });
  assert.deepEqual(got[0].b, { x: 520, y: H - 380 });
});

test("measureMode on: EXTENT ON ONE AXIS ALONE commits — a pure Δt or Δf, unlike a region", () => {
  const vertical: SurfaceRegion[] = [];
  const h1 = harness({ measureMode: true, onMeasure: (r) => vertical.push(r) });
  stroke(h1, MID, [{ x: MID.x, y: 500 }]); // no x travel at all
  assert.equal(vertical.length, 1, "a pure vertical stroke is still a real measurement");

  const horizontal: SurfaceRegion[] = [];
  const h2 = harness({ measureMode: true, onMeasure: (r) => horizontal.push(r) });
  stroke(h2, MID, [{ x: 600, y: MID.y }]); // no y travel at all
  assert.equal(horizontal.length, 1, "a pure horizontal stroke is still a real measurement");
});

test("Shift+drag NEVER changes meaning, in ANY tool mode (docs/23 §10.4): it still marks a region while measuring", () => {
  const regions: SurfaceRegion[] = [];
  const measures: SurfaceRegion[] = [];
  const h = harness({ measureMode: true, onRegion: (r) => regions.push(r), onMeasure: (r) => measures.push(r) });
  stroke(h, MID, [{ x: 460, y: 340 }], { shift: true });
  assert.equal(regions.length, 1, "a tool mode re-binds only the BARE drag; Shift+drag is untouched by it");
  assert.deepEqual(measures, [], "…so no measurement is ALSO produced by the same stroke");
});

test("measureMode off: a plain drag still pans, exactly as before this ticket", () => {
  const h = harness({ measureMode: false, onMeasure: () => { throw new Error("must not fire"); } });
  stroke(h, MID, [{ x: 460, y: 340 }]);
  assert.deepEqual(h.calls.map((c) => c.fn), ["drag"]);
});

test("measureMode: a TAP commits nothing, does not focus, and the band is retracted on release/cancel", () => {
  const got: SurfaceRegion[] = [];
  const clicks: unknown[] = [];
  const h = harness({ measureMode: true, onMeasure: (r) => got.push(r), onClick: (p) => clicks.push(p) });
  stroke(h, MID, [{ x: 402, y: 302 }, { x: 403, y: 301 }]);
  assert.deepEqual(got, [], "a tap must never mark out a measurement");
  assert.deepEqual(clicks, [], "…nor fall through to the click that focuses a row");
  assert.deepEqual(h.calls, [], "…nor pan");

  const seen: (SurfaceRegion | null)[] = [];
  const h2 = harness({ measureMode: true, onMeasure: () => {}, onMeasureDrag: (r) => seen.push(r) });
  stroke(h2, MID, [{ x: 460, y: 340 }]);
  assert.equal(seen[seen.length - 1], null, "the band must be retracted on release");

  const c: (SurfaceRegion | null)[] = [];
  const h3 = harness({ measureMode: true, onMeasure: () => {}, onMeasureDrag: (r) => c.push(r) });
  h3.fire("pointerdown", { button: 0, clientX: MID.x, clientY: MID.y, pointerId: 1 });
  h3.fire("pointermove", { buttons: 1, clientX: 500, clientY: 400 });
  h3.fire("pointercancel", {});
  assert.equal(c[c.length - 1], null, "a cancelled stroke must retract its band too");
});

test("measureMode: T-340's control — NO call reaches the network over the whole vocabulary", () => {
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  const net: unknown[] = [];
  g.fetch = (...args: unknown[]) => { net.push(args); return Promise.reject(new Error("a gesture must not reach the network")); };
  const measures: SurfaceRegion[] = [];
  try {
    const h = harness({ measureMode: true, onMeasure: (r) => measures.push(r), onMeasureDrag: () => {}, onClick: () => {}, onHover: () => {} });
    stroke(h, MID, [{ x: 700, y: 500 }]);
    stroke(h, MID, [{ x: 401, y: 301 }]); // tap
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(net, [], "a measurement gesture reached the network");
  assert.equal(measures.length, 1, "no measurement was produced at all, so the run proves nothing");
});

// ---------------------------------------------------------------------------
// T-824 (docs/23 §10.5): touch. The view/device line must survive a finger: pinch = zoom (view),
// two-finger drag = pan (view), hold-then-drag = region select (the input to the retune OFFER, never
// a retune), long-press = the context menu. A quick finger drag still pans, and a tap still selects.
// ---------------------------------------------------------------------------

/** A finger: `pointerType: "touch"`, with the event's own `timeStamp` (the hold is read off it). */
const finger = (id: number, x: number, y: number, t: number, extra: Record<string, unknown> = {}) =>
  ({ button: 0, buttons: 1, pointerType: "touch", pointerId: id, clientX: x, clientY: y, timeStamp: t, ...extra });

test("touch: a finger that moves at once pans; one HELD first marks out a region and pans nothing", () => {
  const got: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => got.push(r), onRegionDrag: () => {} });
  h.fire("pointerdown", finger(1, 400, 300, 1000));
  h.fire("pointermove", finger(1, 402, 301, 1020)); // under DRAG_PX: nothing moves yet
  assert.deepEqual(h.calls, [], "a finger's first jitter must not pan (a tap stays a tap)");
  h.fire("pointermove", finger(1, 460, 340, 1060));
  h.fire("pointermove", finger(1, 520, 380, 1100));
  h.fire("pointerup", finger(1, 520, 380, 1120, { buttons: 0 }));
  assert.deepEqual(h.calls.map((c) => c.fn), ["drag", "drag"], "a quick finger drag pans");
  // The catch-up drag starts from the PRESS, so the data under the finger stays under it.
  assert.deepEqual(h.calls[0].args, [60, -40]);
  assert.deepEqual(got, [], "…and commits no region");

  const h2 = harness({ onRegion: (r) => got.push(r), onRegionDrag: () => {} });
  h2.fire("pointerdown", finger(1, 400, 300, 1000));
  h2.fire("pointermove", finger(1, 401, 300, 1000 + HOLD_TO_MARK_MS + 10)); // still resting
  h2.fire("pointermove", finger(1, 460, 340, 1000 + HOLD_TO_MARK_MS + 50));
  h2.fire("pointermove", finger(1, 520, 380, 1000 + HOLD_TO_MARK_MS + 90));
  h2.fire("pointerup", finger(1, 520, 380, 1000 + HOLD_TO_MARK_MS + 100, { buttons: 0 }));
  assert.deepEqual(h2.calls, [], "a region stroke moved the viewport");
  assert.equal(got.length, 1, "a held finger's drag commits exactly one region");
  assert.deepEqual(got[0].a, { x: 400, y: H - 300 }, "the region starts at the PRESS, not where it was decided");
  assert.deepEqual(got[0].b, { x: 520, y: H - 380 });
});

test("touch: the meaning is latched — a pan that later pauses never becomes a region", () => {
  const got: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => got.push(r), onRegionDrag: () => {} });
  h.fire("pointerdown", finger(1, 400, 300, 0));
  h.fire("pointermove", finger(1, 440, 300, 50));
  h.fire("pointermove", finger(1, 441, 300, 50 + 5 * HOLD_TO_MARK_MS)); // rests mid-pan
  h.fire("pointermove", finger(1, 500, 360, 60 + 5 * HOLD_TO_MARK_MS));
  h.fire("pointerup", finger(1, 500, 360, 70 + 5 * HOLD_TO_MARK_MS, { buttons: 0 }));
  assert.deepEqual(got, []);
  assert.ok(h.calls.every((c) => c.fn === "drag") && h.calls.length === 3);
});

test("touch: a tap selects; a long-press without travel is the context menu, at the press", () => {
  const clicks: unknown[] = [], menus: unknown[] = [];
  const h = harness({ onClick: (p) => clicks.push(p), onContext: (p) => menus.push(p), onRegion: () => {} });
  h.fire("pointerdown", finger(1, 400, 300, 0));
  h.fire("pointerup", finger(1, 402, 301, 80, { buttons: 0 }));
  assert.equal(clicks.length, 1, "a tap is a click");
  assert.deepEqual(menus, []);
  // The browser's own mid-hold `contextmenu` is swallowed while the finger is down…
  let prevented = false;
  h.fire("pointerdown", finger(1, 400, 300, 1000));
  h.fire("contextmenu", { clientX: 400, clientY: 300, preventDefault: () => { prevented = true; } });
  assert.ok(prevented, "the browser menu must not open over a finger that may still drag a region");
  assert.deepEqual(menus, [], "…and not acted on mid-hold");
  h.fire("pointerup", finger(1, 401, 300, 1000 + HOLD_TO_MARK_MS + 1, { buttons: 0 }));
  assert.deepEqual(menus, [{ x: 400, y: H - 300 }], "…the release of a long-press opens it, at the press");
  assert.equal(clicks.length, 1, "a long-press is not also a click");
  assert.deepEqual(h.calls, []);
});

test("touch: two fingers pinch — spreading zooms IN about their midpoint, and moving them pans", () => {
  const regions: SurfaceRegion[] = [];
  const h = harness({ onRegion: (r) => regions.push(r), onRegionDrag: () => {} });
  h.fire("pointerdown", finger(1, 300, 300, 0));
  h.fire("pointerdown", finger(2, 500, 300, 10)); // spread 200
  h.fire("pointermove", finger(2, 700, 300, 30)); // spread 400, midpoint +100 px
  assert.deepEqual(h.calls.map((c) => c.fn), ["wheel", "drag"]);
  assert.equal(h.calls[0].args[0], 0.5, "fingers twice as far apart halve the span (factor < 1 zooms in)");
  assert.deepEqual(h.calls[1].args, [100, -0], "the midpoint's travel pans the view with it");
  h.fire("pointerup", finger(2, 700, 300, 40, { buttons: 0 }));
  assert.deepEqual(h.settles, ["p0"], "a pinch commits its follow/pause decision when it ends (T-486)");
  // The finger left down is inert: carrying it on as a pan would jump the view.
  h.fire("pointermove", finger(1, 200, 200, 50));
  h.fire("pointerup", finger(1, 200, 200, 60, { buttons: 0 }));
  assert.equal(h.calls.length, 2, "the finger left after a pinch moved the view");
  assert.deepEqual(regions, []);

  // A held first finger's region stroke is ABANDONED by a second finger, never committed.
  const seen: (SurfaceRegion | null)[] = [];
  const h2 = harness({ onRegion: (r) => regions.push(r), onRegionDrag: (r) => seen.push(r) });
  h2.fire("pointerdown", finger(1, 300, 300, 0));
  h2.fire("pointermove", finger(1, 360, 340, HOLD_TO_MARK_MS + 5));
  h2.fire("pointerdown", finger(2, 500, 300, HOLD_TO_MARK_MS + 10));
  assert.equal(seen[seen.length - 1], null, "the pending region box is retracted");
  h2.fire("pointerup", finger(1, 360, 340, HOLD_TO_MARK_MS + 20, { buttons: 0 }));
  h2.fire("pointerup", finger(2, 500, 300, HOLD_TO_MARK_MS + 30, { buttons: 0 }));
  assert.deepEqual(regions, [], "a region was committed by a stroke a pinch took over");
});

test("touch: pinchZoom/touchIntent are the one statement of what a finger means", () => {
  assert.equal(pinchZoom(100, 200).factor, 0.5);
  assert.equal(pinchZoom(200, 100).factor, 2);
  assert.equal(pinchZoom(0, 0).factor, 1, "two fingers meeting cannot divide by zero");
  assert.equal(pinchZoom(10_000, 1).factor, 4, "one event is clamped like a wheel");
  assert.deepEqual(pinchZoom(1, 2).axes, { freq: true, time: true }, "a pinch is uniform, like a plain wheel");
  assert.equal(touchIntent(HOLD_TO_MARK_MS - 1), "pan");
  assert.equal(touchIntent(HOLD_TO_MARK_MS), "region");
  assert.equal(touchIntent(Number.NaN), "pan");
});

test("touch: T-340's control — pinch, pan, hold-drag, tap and long-press reach NO network", () => {
  const g = globalThis as { fetch?: unknown };
  const real = g.fetch;
  const net: unknown[] = [];
  g.fetch = (...args: unknown[]) => { net.push(args); return Promise.reject(new Error("a gesture must not reach the network")); };
  const regions: SurfaceRegion[] = [];
  try {
    const h = harness({ onRegion: (r) => regions.push(r), onRegionDrag: () => {}, onClick: () => {}, onContext: () => {}, onHover: () => {} });
    h.fire("pointerdown", finger(1, 300, 300, 0));
    h.fire("pointerdown", finger(2, 500, 300, 5));
    h.fire("pointermove", finger(2, 600, 350, 10));
    h.fire("pointerup", finger(2, 600, 350, 15, { buttons: 0 }));
    h.fire("pointerup", finger(1, 300, 300, 20, { buttons: 0 }));
    h.fire("pointerdown", finger(1, 400, 300, 100));
    h.fire("pointermove", finger(1, 480, 360, 110));
    h.fire("pointerup", finger(1, 480, 360, 120, { buttons: 0 }));
    h.fire("pointerdown", finger(1, 400, 300, 1000));
    h.fire("pointermove", finger(1, 480, 360, 2000));
    h.fire("pointerup", finger(1, 480, 360, 2010, { buttons: 0 }));
    h.fire("pointerdown", finger(1, 400, 300, 3000));
    h.fire("pointerup", finger(1, 400, 300, 3010, { buttons: 0 }));
    h.fire("pointerdown", finger(1, 400, 300, 4000));
    h.fire("pointerup", finger(1, 400, 300, 5000, { buttons: 0 }));
    assert.ok(h.calls.some((c) => c.fn === "wheel") && h.calls.some((c) => c.fn === "drag"), "the vocabulary ran");
  } finally { if (real) g.fetch = real; else delete g.fetch; }
  assert.deepEqual(net, [], "a touch gesture reached the network");
  assert.equal(regions.length, 1, "the hold-drag produced no region, so the run proves nothing");
});
