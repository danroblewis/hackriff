// **T-526: the user-adjustable shadow gain.** Client-only display preference — this file asserts
// the clamp, the round-trip through storage, and that a broken or absent localStorage falls back to
// the shipped default rather than taking the shadow mark's readability down with it. The GLSL/CPU
// rendering side (the gain reaching the ramp) is `surface-honesty.test.ts`'s; the wheel gesture that
// drives this is `surface-input.test.ts`'s.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  SHADOW_GAIN_DEFAULT, SHADOW_GAIN_KEY, SHADOW_GAIN_MAX, SHADOW_GAIN_MIN,
  clampShadowGain, loadShadowGain, saveShadowGain, shadowGainWheelHandler, stepShadowGain,
} from "../src/surface/shadow-gain";
import { SHADOW_MARK } from "../src/surface/cellrule";

// ---------------------------------------------------------------------------
// A fake localStorage — the same shape T-522's tests already use.
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

test("T-526: the default matches SHADOW_MARK's own gain — a fresh install renders exactly what it always did", () => {
  assert.equal(SHADOW_GAIN_DEFAULT, SHADOW_MARK.gain);
});

test("T-526: clampShadowGain holds [0.05, 0.7] and rejects non-finite input to the default", () => {
  assert.equal(clampShadowGain(0.25), 0.25, "inside the range: unchanged");
  assert.equal(clampShadowGain(0.03), SHADOW_GAIN_MIN, "below the floor: clamped up");
  assert.equal(clampShadowGain(0.05), SHADOW_GAIN_MIN, "the floor itself: kept");
  assert.equal(clampShadowGain(0.7), SHADOW_GAIN_MAX, "the ceiling itself: kept");
  assert.equal(clampShadowGain(5), SHADOW_GAIN_MAX, "above the ceiling: clamped down");
  for (const bad of [NaN, Infinity, -Infinity]) {
    assert.equal(clampShadowGain(bad), SHADOW_GAIN_DEFAULT, `non-finite (${bad}) falls back to the default`);
  }
});

test("T-526: stepShadowGain moves multiplicatively and stays clamped at either end", () => {
  const up = stepShadowGain(0.25, 1);
  assert.ok(up > 0.25 && up <= SHADOW_GAIN_MAX, "a positive notch brightens");
  const down = stepShadowGain(0.25, -1);
  assert.ok(down < 0.25 && down >= SHADOW_GAIN_MIN, "a negative notch dims");
  assert.equal(stepShadowGain(SHADOW_GAIN_MAX, 1), SHADOW_GAIN_MAX, "cannot step past the ceiling");
  assert.equal(stepShadowGain(SHADOW_GAIN_MIN, -1), SHADOW_GAIN_MIN, "cannot step past the floor");
  // Many notches in one direction still lands exactly on the clamp, not beyond it.
  assert.equal(stepShadowGain(0.25, 100), SHADOW_GAIN_MAX);
  assert.equal(stepShadowGain(0.25, -100), SHADOW_GAIN_MIN);
});

test("T-526: the gain round-trips through storage, defaults to SHADOW_MARK's gain, and survives a throwing or absent localStorage", () => {
  let restore = installFakeStorage("ok");
  try {
    assert.equal(loadShadowGain(), SHADOW_GAIN_DEFAULT, "nothing written yet: the shipped default");
    saveShadowGain(0.4);
    assert.equal(loadShadowGain(), 0.4, "round-trips");
    saveShadowGain(50); // out of range — the write itself clamps
    assert.equal(loadShadowGain(), SHADOW_GAIN_MAX, "a write clamps before it ever reaches storage");
  } finally { restore(); }

  restore = installFakeStorage("throws");
  try {
    assert.equal(loadShadowGain(), SHADOW_GAIN_DEFAULT, "a throwing getItem falls back to the default");
    assert.doesNotThrow(() => saveShadowGain(0.4), "a throwing setItem must not propagate");
  } finally { restore(); }

  restore = installFakeStorage("missing");
  try {
    assert.equal(loadShadowGain(), SHADOW_GAIN_DEFAULT, "no localStorage at all: still falls back to the default");
    assert.doesNotThrow(() => saveShadowGain(0.4));
  } finally { restore(); }

  restore = installFakeStorage("ok");
  try {
    (globalThis as { localStorage: Storage }).localStorage.setItem(SHADOW_GAIN_KEY, "not-a-number");
    assert.equal(loadShadowGain(), SHADOW_GAIN_DEFAULT, "unparsable stored value falls back to the default");
  } finally { restore(); }
});

test("T-526: shadowGainWheelHandler sets the surface's gain AND persists it, clamped", () => {
  const restore = installFakeStorage("ok");
  try {
    const surface = { shadowGain: 0.25, setShadowGain(g: number) { this.shadowGain = g; } };
    const handle = shadowGainWheelHandler(surface);
    handle(1);
    assert.ok(surface.shadowGain > 0.25);
    assert.equal(loadShadowGain(), surface.shadowGain, "the handler persisted exactly what it set");
  } finally { restore(); }
});
