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
import { dragIntent } from "../src/surface/preview";
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
    wheel: () => calls.push({ fn: "wheel", args: [] }),
    wheelMap: () => calls.push({ fn: "wheelMap", args: [] }),
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
