// T-362 (the user's bug from live testing, 2026-09-16): the presence boxes drifted and then jumped,
// because T-261 recomputed their top/height only on the ~1 s data poll while the WebGL2 waterfall
// scrolls every animation frame. T-337's mapping was already right; the clock it ran on was wrong.
//
// So these tests are about **time**, not about placement at rest. The one that matters advances the
// waterfall by N rows WITHOUT a data poll and asserts the box moved by exactly N rows: a test that
// only checked placement at poll boundaries passes on the broken code. Its control is a poll that
// delivers no change, which must move nothing — that is the "jump" half of the bug, and it is what
// proves a poll is no longer a placement event.
//
// They drive the real `Waterfall` through a recording WebGL2 stub, so what is asserted is the rect
// the render pass actually submitted, in the frame it submitted it, next to the row pass's own
// head — not a helper called in the same style the renderer is hoped to use.
import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { rowsBackAt } from "../src/axis";
import { boxAt, placeTimeBoxes, type BoxStyle, type TimeBox } from "../src/timebox";
import { BOX_MIN_PX, Waterfall } from "../src/waterfall";

const ROWS = 512; // Waterfall's ring depth
const BINS = 64;
const W = 800, H = 600;
const SPEC_FRAC = 0.35;
const PANE_H = H - Math.floor(H * SPEC_FRAC);

const STYLE: BoxStyle = {
  fill: [0, 1, 0, 0.1], border: [0, 1, 0, 1], top: [1, 0.5, 0, 1], borderPx: 1, topPx: 2, dashPx: 0, hatch: false,
};

// ---- a recording WebGL2 stub ---------------------------------------------------------------
//
// Enough of WebGL2 to run `Waterfall` unchanged: programs learn their uniform names by reading the
// shader sources they were given, uniform writes are remembered per location, and every draw call
// records the program's uniforms as they stood at that instant. Anything not modelled is a no-op,
// and any unknown SCREAMING_CASE property is a distinct constant.

interface Op { kind: "viewport" | "draw"; args: number[]; u?: Record<string, number[]> }

function stubGl() {
  const K: Record<string, number> = {
    VERTEX_SHADER: 1, FRAGMENT_SHADER: 2, COMPILE_STATUS: 3, LINK_STATUS: 4, ACTIVE_UNIFORMS: 5,
    MAX_TEXTURE_SIZE: 6, TEXTURE_2D: 7, RED: 8, FLOAT: 9, UNSIGNED_BYTE: 10, TRIANGLES: 11,
    LINE_STRIP: 12, TRIANGLE_STRIP: 13, TEXTURE0: 100,
  };
  let nextK = 200;
  const ops: Op[] = [];
  const values = new Map<object, number[]>();
  const locs = new Map<string, { p: Prog; name: string }>();
  let current: Prog | null = null;
  interface Prog { id: number; shaders: string[]; uniforms: string[] }
  let nextProg = 0;

  const snapshot = (p: Prog | null): Record<string, number[]> => {
    const out: Record<string, number[]> = {};
    if (p) for (const n of p.uniforms) { const v = values.get(locs.get(`${p.id}/${n}`)!); if (v) out[n] = v; }
    return out;
  };
  const setU = (loc: unknown, ...v: number[]) => { if (loc) values.set(loc as object, v); };

  const base: Record<string, unknown> = {
    drawingBufferWidth: W, drawingBufferHeight: H,
    getParameter: (k: number) => (k === K.MAX_TEXTURE_SIZE ? 4096 : 0),
    getExtension: () => null,
    createTexture: () => ({}), createFramebuffer: () => ({}), createVertexArray: () => ({}),
    createProgram: (): Prog => ({ id: nextProg++, shaders: [], uniforms: [] }),
    createShader: () => ({ src: "" }),
    shaderSource: (s: { src: string }, src: string) => { s.src = src; },
    attachShader: (p: Prog, s: { src: string }) => { p.shaders.push(s.src); },
    getShaderParameter: () => true,
    linkProgram: (p: Prog) => {
      const names = new Set<string>();
      for (const src of p.shaders) {
        for (const m of src.matchAll(/\buniform\s+\w+\s+([^;]+);/g)) {
          for (const part of m[1].split(",")) names.add(part.trim().split(/[\s[]/)[0]);
        }
      }
      p.uniforms = [...names];
    },
    getProgramParameter: (p: Prog, k: number) => (k === K.ACTIVE_UNIFORMS ? p.uniforms.length : true),
    getActiveUniform: (p: Prog, i: number) => ({ name: p.uniforms[i] }),
    getUniformLocation: (p: Prog, name: string) => {
      const key = `${p.id}/${name}`;
      if (!locs.has(key)) locs.set(key, { p, name });
      return locs.get(key)!;
    },
    useProgram: (p: Prog) => { current = p; },
    uniform1i: setU, uniform1f: setU, uniform2f: setU, uniform3f: setU, uniform4f: setU,
    viewport: (...a: number[]) => { ops.push({ kind: "viewport", args: a }); },
    drawArrays: (mode: number, first: number, count: number) => {
      ops.push({ kind: "draw", args: [mode, first, count], u: snapshot(current) });
    },
  };
  const gl = new Proxy(base, {
    get(t, k) {
      if (k in t) return t[k as string];
      const n = String(k);
      if (/^[A-Z][A-Z0-9_]*$/.test(n)) return (K[n] ??= nextK++);
      return () => undefined;
    },
  });
  return { gl, ops, K };
}

/** A `Waterfall` over the stub, plus the frame pump and the row feeder. */
function harness() {
  const { gl, ops } = stubGl();
  const canvas = { getContext: () => gl, clientWidth: W, clientHeight: H, width: 0, height: 0 } as unknown as HTMLCanvasElement;
  const g = globalThis as unknown as Record<string, unknown>;
  let cb: ((t: number) => void) | null = null;
  let now = 0;
  g.window = { devicePixelRatio: 1 };
  g.requestAnimationFrame = (f: (t: number) => void) => { cb = f; return 1; };
  g.cancelAnimationFrame = () => { cb = null; };
  const wf = new Waterfall(canvas, BINS, 25);
  const frame = () => { const f = cb; cb = null; ops.length = 0; now += 16; f?.(now); };
  const push = (tS: number) => wf.push(new Float32Array(BINS).fill(-100), tS);
  /** The rect uniform of the box draw in the frame just run (the box program is the only one with
   * a `uRect`), or null when the pass drew no box. */
  const rect = (): number[] | null => ops.find((o) => o.kind === "draw" && o.u?.uRect)?.u!.uRect ?? null;
  return { wf, ops, frame, push, rect };
}

const P = 0.04; // row period, s
const T0 = 1000;
const tOf = (k: number) => T0 + k * P;

/** Pushes each row time and runs a frame at least every 16 of them (`MAX_ROWS_PER_FRAME`: a frame
 * that finds more than that queued drops the excess, exactly as a real backlog would). */
function feedTimes(h: ReturnType<typeof harness>, times: readonly number[]) {
  times.forEach((t, i) => { h.push(t); if ((i + 1) % 16 === 0) h.frame(); });
  h.frame();
}

/** Rows `from`..`to-1` of the steady-period scene. */
const feed = (h: ReturnType<typeof harness>, from: number, to: number) =>
  feedTimes(h, Array.from({ length: to - from }, (_, i) => tOf(from + i)));

const box = (tLo: number, tHi: number, u0 = 0.4, u1 = 0.6): TimeBox => ({ id: "a", u0, u1, tLo, tHi, style: STYLE });
/** NDC → fraction of the waterfall pane, top = 0 (the newest row). */
const yFrac = (ndc: number) => (1 - ndc) / 2;
const near = (a: number, b: number, eps = 1e-9) => assert.ok(Math.abs(a - b) <= eps, `${a} ≉ ${b}`);

// ---- the bug --------------------------------------------------------------------------------

test("T-362: the box tracks the scroll BETWEEN polls — N rows in, N rows down, with no data poll at all", () => {
  const h = harness();
  feed(h, 0, 40); // newest row is k=39
  // One poll, at row 39. The box covers rows k=10..20 of capture time.
  h.wf.setBoxes([box(tOf(10), tOf(20))]);
  h.frame();
  const before = h.rect()!;
  assert.ok(before, "the render pass drew the box");
  near(yFrac(before[3]), 20 / ROWS, 1e-6); // newest edge: 20 rows back
  near(yFrac(before[1]), 30 / ROWS, 1e-6); // older edge: 30 rows back

  // Now the waterfall scrolls. NO setBoxes: this is exactly the between-polls window in which the
  // old code left the box standing still while the rows moved out from under it.
  const N = 8;
  feed(h, 40, 40 + N);
  const after = h.rect()!;
  near(yFrac(after[3]), (20 + N) / ROWS, 1e-6);
  near(yFrac(after[1]), (30 + N) / ROWS, 1e-6);
  // And it is the rows' OWN mapping, not a nominal rate: what the ring says, to the pixel.
  near(yFrac(after[3]), h.wf.rowsBackAt(tOf(20)) / ROWS, 1e-6);
  // Horizontal placement is untouched by time passing.
  assert.deepEqual([after[0], after[2]], [before[0], before[2]]);
});

test("T-362 control: a poll that delivers no change produces NO jump — the position is identical before and after", () => {
  const h = harness();
  feed(h, 0, 40);
  h.wf.setBoxes([box(tOf(10), tOf(20))]);
  h.frame();
  const a = h.rect()!;
  // The poll fires again with the very same interval. Under the old code this was the moment the
  // box snapped to where it should have been all along; now it is a data update that moves nothing.
  h.wf.setBoxes([box(tOf(10), tOf(20))]);
  h.frame();
  assert.deepEqual(h.rect(), a, "an unchanged poll moved the box");
  // And several frames with no poll at all, while nothing new arrives, are equally still.
  h.frame(); h.frame(); h.frame();
  assert.deepEqual(h.rect(), a);
});

test("T-362 paused: the view holds while capture continues — the box stays on the rows ON SCREEN, not the newest ones", () => {
  // T-339: pause freezes the view, never the capture. The client stops pushing rows into the
  // waterfall while the ring keeps filling, so the head stands still — and the box must stand still
  // with it, on the displayed rows, however long the pause lasts and however many polls land.
  const h = harness();
  feed(h, 0, 40);
  h.wf.setBoxes([box(tOf(10), tOf(20))]);
  h.frame();
  const held = h.rect()!;
  for (let i = 0; i < 30; i++) {
    if (i % 10 === 0) h.wf.setBoxes([box(tOf(10), tOf(20))]); // polls keep arriving while paused
    h.frame();
    assert.deepEqual(h.rect(), held, `frame ${i} of the pause moved the box`);
  }
});

test("T-362 zoomed: the box goes through the same zoom window as the rows, in the same frame", () => {
  const h = harness();
  feed(h, 0, 40);
  h.wf.setBoxes([box(tOf(10), tOf(20), 0.45, 0.55)]);
  h.wf.setView(0.4, 0.6);
  h.frame();
  const r = h.rect()!;
  // u=0.45 and 0.55 of the band are a quarter and three quarters of a [0.4, 0.6] window.
  near((r[0] + 1) / 2, 0.25, 1e-6);
  near((r[2] + 1) / 2, 0.75, 1e-6);
  // The row pass in that same frame was handed the identical window: one frequency mapping.
  const wfDraw = h.ops.find((o) => o.kind === "draw" && o.u?.uU0 && o.u?.uHead)!;
  assert.deepEqual([wfDraw.u!.uU0[0], wfDraw.u!.uU1[0]], [0.4, 0.6]);
  // Zooming further scales the box exactly as it scales the rows, with no poll in between.
  h.wf.setView(0.45, 0.55);
  h.frame();
  const z = h.rect()!;
  near((z[0] + 1) / 2, 0, 1e-6);
  near((z[2] + 1) / 2, 1, 1e-6);
  // And time is untouched by a frequency zoom.
  assert.deepEqual([z[1], z[3]], [r[1], r[3]]);
});

test("T-362: the box is drawn in the row pass's own viewport, between the rows and the spectrum", () => {
  const h = harness();
  feed(h, 0, 40);
  h.wf.setBoxes([box(tOf(10), tOf(20))]);
  h.frame();
  const i = h.ops.findIndex((o) => o.kind === "draw" && o.u?.uHead); // the row pass
  const j = h.ops.findIndex((o) => o.kind === "draw" && o.u?.uRect); // the box pass
  assert.ok(i >= 0 && j > i, "the box is drawn after the rows");
  // No viewport change between them: the boxes are laid out over exactly the pane the rows filled.
  assert.ok(!h.ops.slice(i, j).some((o) => o.kind === "viewport"), "a viewport change came between them");
  assert.ok(h.ops.slice(j).some((o) => o.kind === "viewport"), "the spectrum pane still follows");
  // The pane the rect is expressed in is the one that pass set: the waterfall's, not the canvas's.
  const vp = h.ops.slice(0, i).filter((o) => o.kind === "viewport").pop()!;
  assert.deepEqual(vp.args, [0, 0, W, PANE_H]);
});

test("T-362: uneven rows — a gated run advances capture time without advancing the ring, and the box follows the ring", () => {
  // The drift T-337 measured, now in the between-polls window: a nominal rows-per-second would put
  // the box at (age × rate) rows back, which after a 1.2 s gap is 30 rows adrift on a 512-row pane.
  const h = harness();
  const times = [...Array(40)].map((_, k) => (k < 20 ? T0 + k * P : T0 + 1.2 + k * P));
  feedTimes(h, times);
  h.wf.setBoxes([{ id: "a", u0: 0.4, u1: 0.6, tLo: times[2], tHi: times[12], style: STYLE }]);
  h.frame();
  const r = h.rect()!;
  // times[12] is the boundary at rows-back 40−12 = 28 (row k is 39−k rows back; the boundary below
  // row j is at position j+1).
  near(yFrac(r[3]), 28 / ROWS, 1e-6);
  const nominalBack = (times[39] - times[12]) * 25; // what a declared 25 rows/s would have said
  assert.ok(nominalBack - 28 > 28, `a nominal rate is ${nominalBack - 28} rows out — ~6 % of the pane`);
});

// ---- the placement arithmetic ---------------------------------------------------------------

test("placeTimeBoxes: a box off the rows held draws nothing; one off the side is clipped, not moved", () => {
  const times = [...Array(64)].map((_, k) => T0 - k * P);
  const back = (t: number) => rowsBackAt((k) => times[k] ?? NaN, times.length, t);
  assert.deepEqual(placeTimeBoxes([box(0, 1)], back, ROWS, 0, 1), [], "scrolled off: no clamped rectangle");
  const [p] = placeTimeBoxes([box(times[20], times[10], -0.2, 0.1)], back, ROWS, 0, 1);
  assert.equal(p.x0, -0.2, "the off-screen edge keeps its true position; the viewport clips it");
  near(p.x1, 0.1);
});

test("placeTimeBoxes: a sub-pixel box is widened about its own centre, never moved off it", () => {
  const times = [...Array(64)].map((_, k) => T0 - k * P);
  const back = (t: number) => rowsBackAt((k) => times[k] ?? NaN, times.length, t);
  const min = BOX_MIN_PX / W;
  const [p] = placeTimeBoxes([box(times[11], times[10], 0.5, 0.5000001)], back, ROWS, 0, 1, min, 0);
  near((p.x0 + p.x1) / 2, 0.50000005, 1e-7);
  near(p.x1 - p.x0, min, 1e-12);
});

test("boxAt: hit-testing is containment in the boxes' own axes, topmost first — never a remembered rectangle", () => {
  const under = box(T0, T0 + 1, 0.3, 0.7);
  const over: TimeBox = { ...box(T0, T0 + 1, 0.4, 0.6), id: "b" };
  assert.equal(boxAt([under, over], 0.5, T0 + 0.5)?.id, "b", "the last drawn wins");
  assert.equal(boxAt([under, over], 0.35, T0 + 0.5)?.id, "a");
  assert.equal(boxAt([under, over], 0.5, T0 + 2), null, "outside the time extent");
  assert.equal(boxAt([under, over], 0.1, T0 + 0.5), null, "outside the frequency extent");
  assert.equal(boxAt([under], 0.5, NaN), null, "a pointer off the rows resolves to nothing");
});

/** The thin-client rule (CLAUDE.md) and T-337's own finding: the overlay must never carry an RF
 * constant or a rows-per-second. A nominal rate would look right for the first second after every
 * poll and drift linearly with age — exactly the bug this file exists for, re-introduced. */
test("T-362: no signal logic, no RF constant and no nominal rate in the box module", () => {
  const src = readFileSync("src/timebox.ts", "utf8").replace(/\/\*[\s\S]*?\*\//g, "").replace(/^\s*\/\/.*$/gm, "");
  for (const word of ["_db", "dbfs", "snr", "occupancy", "noise", "e6", "e9", "rowRate", "rowsPerS", "rowPeriod", "sample_rate", "Date.now"]) {
    assert.ok(!src.toLowerCase().includes(word.toLowerCase()), `src/timebox.ts must not contain "${word}"`);
  }
  assert.ok(!/\b\d{6,}(\.\d+)?\b/.test(src), "src/timebox.ts must not contain a hard-coded frequency");
});
