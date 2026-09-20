// **T-522: the found-signal overlay toggle.** The user finds the Candidate/Confirmed boxes
// cluttered sometimes; this is a pure client display preference — it must change nothing about
// what is fetched, polled or detected, only which boxes `paneMarkBoxes` hands the render pass.
//
// `paneMarkBoxes` (`ui/src/app/centre/surface.ts`) is the same composition `boxesFor` calls inside
// `SurfaceView.frame()`, so asserting against it is asserting on what the render path is actually
// asked to draw — not on a boolean read off to the side (the same discipline `surface-marks.test.ts`
// holds over `markQuads`).
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { CANDIDATE_MARK, CONFIRMED_MARK } from "../src/surface/marks";
import type { Box } from "../src/surface/lattice";
import { paneMarkBoxes, readShowSignals, writeShowSignals } from "../src/app/centre/surface";

const S = 1e9;
const PANE_BOX: Box = { f0Hz: 99.6e6, f1Hz: 102e6, t0Ns: 0, t1Ns: 20 * S };

const row = (o: Record<string, unknown>) => ({ id: "x", state: "confirmed", f_lo_hz: 1e6, f_hi_hz: 2e6, ...o }) as never;
const ROWS = [
  row({ id: "conf", presence: { last_interval: { t_start_s: 1, t_end_s: 8, open: false } } }),
  row({ id: "cand", state: "candidate", presence: { last_interval: { t_start_s: 2, t_end_s: 9, open: true } } }),
];
const SEL = [{ id: "s1", f_lo: 100e6, f_hi: 100.2e6, t_lo: 1, t_hi: 2 }];

// ---------------------------------------------------------------------------
// A fake localStorage, so the test controls what "storage" does without touching the real one.
// ---------------------------------------------------------------------------

function installFakeStorage(behavior: "ok" | "throws" | "missing") {
  const orig = (globalThis as { localStorage?: Storage }).localStorage;
  if (behavior === "missing") {
    // @ts-expect-error deliberately deleting the global to simulate its absence
    delete (globalThis as { localStorage?: Storage }).localStorage;
  } else if (behavior === "throws") {
    (globalThis as { localStorage: Storage }).localStorage = {
      getItem() { throw new Error("storage disabled"); },
      setItem() { throw new Error("storage disabled"); },
    } as unknown as Storage;
  } else {
    const store = new Map<string, string>();
    (globalThis as { localStorage: Storage }).localStorage = {
      getItem: (k: string) => (store.has(k) ? store.get(k)! : null),
      setItem: (k: string, v: string) => { store.set(k, v); },
      removeItem: (k: string) => { store.delete(k); },
      clear: () => store.clear(),
      key: () => null,
      get length() { return store.size; },
    } as unknown as Storage;
  }
  return () => { (globalThis as { localStorage?: Storage }).localStorage = orig; };
}

// ---------------------------------------------------------------------------
// The render path: what paneMarkBoxes actually hands the pass
// ---------------------------------------------------------------------------

test("T-522: OFF drops every signal-box from what the pane draws; selections and the pending region are unaffected", () => {
  const pendingRegion = { f0Hz: 100e6, f1Hz: 100.5e6, t0Ns: 3 * S, t1Ns: 5 * S };
  const on = paneMarkBoxes(ROWS, null, SEL, null, PANE_BOX, pendingRegion, true);
  const off = paneMarkBoxes(ROWS, null, SEL, null, PANE_BOX, pendingRegion, false);

  assert.deepEqual(on.filter((b) => b.kind === "signal-box").map((b) => b.id).sort(), ["cand", "conf"]);
  assert.deepEqual(off.filter((b) => b.kind === "signal-box"), [], "OFF: no signal-box reaches the draw list");

  // Nothing else in the composition moves: same selection box, same pending-region box, same count
  // difference as exactly the two signal boxes removed.
  assert.equal(on.filter((b) => b.kind === "selection-box").length, 1);
  assert.equal(off.filter((b) => b.kind === "selection-box").length, 1);
  assert.equal(on.filter((b) => b.kind === "pending-region").length, 1);
  assert.equal(off.filter((b) => b.kind === "pending-region").length, 1);
  assert.equal(on.length - off.length, 2);
});

test("T-522: re-enabling restores exactly the same boxes signalMarkBoxes would draw (colour, focus, ink included)", () => {
  const on = paneMarkBoxes(ROWS, "cand", SEL, null, PANE_BOX, null, true);
  const conf = on.find((b) => b.id === "conf")!;
  const cand = on.find((b) => b.id === "cand")!;
  assert.deepEqual(conf.rgba, CONFIRMED_MARK);
  assert.deepEqual(CANDIDATE_MARK.slice(0, 3), cand.rgba.slice(0, 3));
  assert.equal(cand.rgba[3], 1, "the focused row is still the opaque one — the toggle changes no other rule");
});

// ---------------------------------------------------------------------------
// The pref: round-trips through storage, and falls back to SHOWN
// ---------------------------------------------------------------------------

test("T-522: the pref round-trips through storage, defaults to shown, and survives a throwing or absent localStorage", () => {
  let restore = installFakeStorage("ok");
  try {
    assert.equal(readShowSignals(), true, "nothing written yet: default is shown");
    writeShowSignals(false);
    assert.equal(readShowSignals(), false, "round-trips OFF");
    writeShowSignals(true);
    assert.equal(readShowSignals(), true, "round-trips back ON");
  } finally { restore(); }

  restore = installFakeStorage("throws");
  try {
    assert.equal(readShowSignals(), true, "a throwing getItem falls back to shown");
    assert.doesNotThrow(() => writeShowSignals(false), "a throwing setItem must not propagate");
  } finally { restore(); }

  restore = installFakeStorage("missing");
  try {
    assert.equal(readShowSignals(), true, "no localStorage at all: still falls back to shown");
    assert.doesNotThrow(() => writeShowSignals(false));
  } finally { restore(); }
});

// ---------------------------------------------------------------------------
// It reaches no network route
// ---------------------------------------------------------------------------

test("T-522: the toggle is pure presentation — the source names no route, and touches no inventory state", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  // The toggle's own code is two blocks: the persisted-preference helpers and the click handler.
  // They are sliced separately rather than as one span from the first to the second, because
  // T-506 re-homed the capture clock and the ring rules into the lines between them — unrelated
  // code whose comments legitimately name `GET /api/timeline`. Asserting on the span would make
  // this guard fail for whatever a neighbour does; asserting on the blocks is the actual claim.
  const helpers = src.slice(src.indexOf("SHOW_SIGNALS_KEY"), src.indexOf("function mount("));
  const handler = src.slice(src.indexOf("signalsBtn.addEventListener"), src.indexOf("signalsBtn.addEventListener") + 400);
  for (const [what, region] of [["the preference helpers", helpers], ["the click handler", handler]] as const) {
    assert.ok(!/\/api\//.test(region), `no route is named in ${what}`);
    assert.ok(!/store\.set/.test(region), `${what} writes no store state — it flips a local var only`);
  }
});

test("T-522: the button carries an accessible label and a pressed state", () => {
  const src = readFileSync("src/app/centre/surface.ts", "utf8");
  assert.match(src, /signalsBtn = h\("button",/);
  assert.match(src, /aria-pressed/);
  assert.match(src, /"Signals"/);
});
