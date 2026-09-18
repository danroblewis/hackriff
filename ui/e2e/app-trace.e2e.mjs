// **The spectrum trace, in a real browser, against the bytes the socket delivered** (T-457).
//
// ## The claims this tier makes, and why the unit tier cannot make them
//
// `ui/test/surface-trace.test.ts` proves the trace's *arithmetic*: the time-addressable slice, the
// pooling, the shared axes, the gap where nothing was observed, the strip carved out of the pane.
// Every one of those would still pass if the app never fetched a spectrum row, or fetched one and
// drew a different one — which is exactly the trap this ticket was warned about: **a trace that
// renders is not a trace showing the frame at the pane's time position.** T-450's renderer was
// proved on 114 973 of 115 200 pixels for a module that could not load in a browser; the lesson is
// to name the subject before claiming anything about it.
//
// So this file observes the delivered frame **independently of the client**, by wrapping `WebSocket`
// before any of the page's scripts run and decoding the stream contract's binary record itself
// (§5.2: type byte, i64 µs at offset 16, `f32` row from offset 32 — the same layout
// `ui/src/app/net.ts` reads, restated here **deliberately**, because a tap that imported the
// client's parser could not catch the client mis-parsing). Three different subjects follow:
//
//  1. **The data path.** When the trace says its source is the *live frame*, the peak it states is
//     the peak of a row the socket actually delivered — same dB, same frequency. `sampleFrame`
//     max-pools, so the peak column's value **is** the row's global maximum; an equality, not a
//     bound.
//  2. **The render path.** The composited pixels agree with that statement: the highest point of the
//     drawn trace sits in the screen column the stated peak frequency maps to, through the window
//     the pane itself says it is showing. This is the half a correct readout over a broken renderer
//     would fail.
//  3. **Absence, and that the absence is not vacuous.** The viewport is wider than the tuned band,
//     so one frame of one strip contains both cases. The drawn columns must start and end at the
//     band's edges — the edges being taken from the **header the socket delivered**, not from
//     anything the client said — and every column outside it must be empty. A trace that drew
//     nothing would pass the absence half; one that drew everywhere would pass the presence half;
//     pinning both edges in the same frame is what defeats each cheat with the other.
//
// And the honesty property only a browser can exercise: a pane scrubbed into the past draws the
// slice **at that past instant**, from the pyramid, and says which — rather than laying the current
// frame over a picture of then.
import test from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** Height of the trace strip, device px — `TRACE_PX` in ui/src/app/centre/surface.ts. */
const TRACE_PX = 96;
/** `TRACE_COLUMNS` in ui/src/surface/trace.ts. */
const TRACE_COLUMNS = 256;

/**
 * The tap. Observes only: it forwards nothing, changes nothing, and answers no question the page
 * asks. `class ... extends WebSocket` rather than a wrapping function, so `new`, the prototype chain
 * and every property the app sets (`binaryType`, `onmessage`) behave exactly as they would.
 */
const TAP = `(() => {
  const Base = WebSocket;
  const tap = { headers: 0, rows: 0, geom: null, recent: [] };
  window.__hkTap = tap;
  class TapSocket extends Base {
    constructor(...a) {
      super(...a);
      this.addEventListener("message", (e) => {
        if (typeof e.data === "string") {
          try {
            const h = JSON.parse(e.data);
            if (h && h.kind === "spectrum" && h.bandwidth_hz > 0 && h.fft_size >= 1) {
              tap.headers++;
              tap.geom = { centerHz: h.center_hz, bandwidthHz: h.bandwidth_hz, bins: h.fft_size };
              tap.recent.length = 0; // a retune: the old band's rows are not this band's
            }
          } catch (_) {}
          return;
        }
        const buf = e.data;
        if (!(buf instanceof ArrayBuffer) || buf.byteLength < 36 || !tap.geom) return;
        const dv = new DataView(buf);
        if (dv.getUint8(0) !== 1) return;            // REC_DATA
        if (dv.getUint8(1) & 1) return;              // FLAG_GATED: no samples in this record
        const n = (buf.byteLength - 32) >> 2;
        if (n < 2) return;
        const row = new Float32Array(buf, 32, n);
        let best = -Infinity, at = -1;
        for (let i = 0; i < n; i++) { const v = row[i]; if (Number.isFinite(v) && v > best) { best = v; at = i; } }
        if (at < 0) return;
        const f0 = tap.geom.centerHz - tap.geom.bandwidthHz / 2;
        const binHz = tap.geom.bandwidthHz / n;
        tap.rows++;
        tap.recent.push({
          tS: Number(dv.getBigInt64(16, true) / 1000n) / 1e6,
          bins: n, peakDb: best, peakHz: f0 + (at + 0.5) * binHz,
        });
        if (tap.recent.length > 400) tap.recent.splice(0, tap.recent.length - 400);
      });
    }
  }
  window.WebSocket = TapSocket;
})();`;

/** What the page is saying and what the socket delivered, read in ONE evaluation so they agree. */
const SNAPSHOT = `(() => {
  const row = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
  return JSON.stringify({
    trace: document.querySelector('.sf-trace')?.textContent ?? "",
    headline: row ? row.children[1].textContent : "",
    tap: { headers: window.__hkTap.headers, rows: window.__hkTap.rows, geom: window.__hkTap.geom,
           recent: window.__hkTap.recent.slice(-400) },
  });
})()`;

/** `100.800 MHz ± 1.200 MHz` → the pane's frequency window in Hz. */
function windowOf(headline) {
  const m = /([\d.]+) MHz ± ([\d.]+) (MHz|kHz|Hz)/.exec(headline);
  assert.ok(m, `the pane did not state a frequency window: ${JSON.stringify(headline)}`);
  const centerHz = Number(m[1]) * 1e6;
  const halfHz = Number(m[2]) * (m[3] === "MHz" ? 1e6 : m[3] === "kHz" ? 1e3 : 1);
  return { f0Hz: centerHz - halfHz, f1Hz: centerHz + halfHz, spanHz: 2 * halfHz };
}

/** `slice 12:34:56Z (live frame) · peak -41.2 dB at 100.3021 MHz · max-hold …` */
function statedSlice(trace) {
  const m = /slice ([\d:]+)Z \(([^)]+)\) · peak (-?[\d.]+) dB at ([\d.]+) MHz/.exec(trace);
  assert.ok(m, `the slice stated no peak: ${JSON.stringify(trace)}`);
  return { at: m[1], source: m[2], db: Number(m[3]), hz: Number(m[4]) * 1e6 };
}

/**
 * Where the trace was drawn in the strip, by colour.
 *
 * The strip is the top `TRACE_PX` device px of the canvas — the pane is that much shorter and the
 * map is along the bottom — and the only things drawn there are the two series over `BACKDROP`
 * (rgb 10,10,13). So any bright pixel is ink: blue-dominant is the slice, red-dominant the max-hold.
 */
function strip(img, rect) {
  const x0 = Math.round(rect.x), y0 = Math.round(rect.y), w = Math.round(rect.w);
  const cols = new Array(w).fill(-1);       // topmost slice pixel per column, -1 = none
  let slicePx = 0, holdPx = 0;
  for (let x = 0; x < w; x++) {
    for (let y = 0; y < TRACE_PX; y++) {
      const d = ((y0 + y) * img.width + (x0 + x)) * 4;
      const r = img.data[d], g = img.data[d + 1], b = img.data[d + 2];
      if (r + g + b < 180) continue;
      if (b > r) { slicePx++; if (cols[x] < 0) cols[x] = y; } else if (r > b) holdPx++;
    }
  }
  let peakCol = -1, peakY = Infinity;
  for (let x = 0; x < w; x++) if (cols[x] >= 0 && cols[x] < peakY) { peakY = cols[x]; peakCol = x; }
  return { w, cols, slicePx, holdPx, peakCol, peakY, drawn: cols.filter((v) => v >= 0).length };
}

test("the trace is the spectrum at the viewport's time position, and its numbers are the socket's own", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });

  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load", "the app page never fired load");
  assert.equal(await page.eval("JSON.stringify(window.__cspViolations ?? [])"), "[]",
    "the app violated its own CSP — the tap changes nothing about that");
  assert.deepEqual(page.exceptions, [], "uncaught exception during load");

  // The tap has to be the thing that sees the stream, or everything below is vacuous.
  await page.waitFor("the spectrum socket to deliver rows the tap can see",
    "(window.__hkTap?.rows ?? 0) > 3 && !!window.__hkTap.geom", { timeoutMs: 60000 });

  const { census, rect } = await page.waitForCanvas(".sf-canvas",
    (c) => c.distinct >= 16 && c.dominantShare < 0.97,
    { timeoutMs: 90000, saveAs: path.join(ART, "app-trace.png") });
  t.diagnostic(`canvas ${rect.w}×${rect.h}: ${census.distinct} distinct colours, dominant ${census.dominant}`);

  // ---- (1) the data path, on a frame the socket delivered ----
  //
  // Wait for the source the readout NAMES to be the live frame. The pane's time position is its own
  // window's top, and only there is the delivered row finer than a cell — so this is the state in
  // which the two things being compared are the same thing. Everywhere else the slice legitimately
  // comes from the pyramid, and comparing it to a stream row would be the adjacent-question mistake.
  await page.waitFor("the trace to state a live-frame slice",
    `/slice [\\d:]+Z \\(live frame\\) · peak/.test(document.querySelector('.sf-trace')?.textContent ?? "")`,
    { timeoutMs: 60000 });
  const snap = JSON.parse(await page.eval(SNAPSHOT));
  t.diagnostic(`trace readout: ${snap.trace}`);
  t.diagnostic(`tap: ${snap.tap.headers} headers, ${snap.tap.rows} rows, ${snap.tap.recent.length} retained`);
  assert.ok(snap.tap.recent.length > 0, "the tap retained no row to compare against");

  const slice = statedSlice(snap.trace);
  assert.equal(slice.source, "live frame", "the snapshot caught a different source than the wait did");
  const win = windowOf(snap.headline);
  const colHz = win.spanHz / TRACE_COLUMNS;
  // `sampleFrame` pools by MAX, so the peak column's value is the row's global maximum — an
  // equality, to the one decimal the readout prints. A client that drew a stale row, a different
  // stream, or a mean instead of a max fails here, and each of those is a different bug.
  const matches = snap.tap.recent.filter((r) => Math.abs(r.peakDb - slice.db) < 0.05);
  assert.ok(matches.length > 0,
    `the trace states a live-frame peak of ${slice.db} dB; no row the socket delivered has that maximum.\n` +
    `  recent maxima: ${[...new Set(snap.tap.recent.map((r) => r.peakDb.toFixed(1)))].join(", ")}`);
  assert.ok(matches.some((r) => Math.abs(r.peakHz - slice.hz) <= colHz),
    `the trace puts that peak at ${(slice.hz / 1e6).toFixed(4)} MHz; the rows carrying it peak at ` +
    `${matches.map((r) => (r.peakHz / 1e6).toFixed(4)).join(", ")} MHz (one column is ${(colHz / 1e3).toFixed(1)} kHz)`);
  t.diagnostic(`slice: ${slice.db} dB at ${(slice.hz / 1e6).toFixed(4)} MHz (${slice.source}) — ` +
    `matched ${matches.length}/${snap.tap.recent.length} delivered rows`);

  // The max-hold is beside it — with a peak when the pyramid has answered for this window, and with
  // its own reason when it has not. Requiring a *peak* here would be requiring a tile to be resident
  // at the growing edge, which is a claim about when a node gets built, not about the trace. (That
  // is also the thing about to change under the incremental-tile work, so this tier does not encode
  // today's answer to it.)
  assert.match(snap.trace, /max-hold over [\d.]+ (ms|s|min) (·|—)/,
    "the max-hold series is missing from the readout");
  assert.match(snap.trace, /scale -?[\d.]+ dB … -?[\d.]+ dB, measured from the served tiles/,
    "the trace must say which measured range it is drawn against");

  // ---- (2) the render path: the pixels agree with the statement ----
  const s = strip(await page.shot(path.join(ART, "app-trace-strip.png")), rect);
  assert.ok(s.slicePx > 20, `the slice series drew ${s.slicePx} pixels in the strip — that is not a trace`);
  // The max-hold's pixels are DIAGNOSTIC, not asserted. Whether it draws depends on a tile being
  // resident for this pane's window, which is a claim about when the pyramid materialises a node —
  // not about the trace, and exactly the thing the incremental-tile work is about to change. Its
  // arithmetic is pinned in `ui/test/surface-trace.test.ts`, where the residency is the fixture.
  t.diagnostic(`strip ink: ${s.slicePx} slice px, ${s.holdPx} max-hold px`);

  const expected = ((slice.hz - win.f0Hz) / win.spanHz) * s.w;
  // Tolerance: a drawn sample is a whole pooled column wide, and the live row advances between the
  // snapshot and the screenshot. Three columns of the trace's own grid.
  const tolPx = Math.max(6, (3 * s.w) / TRACE_COLUMNS);
  t.diagnostic(`highest drawn sample at column ${s.peakCol} (row ${s.peakY} of ${TRACE_PX}); ` +
    `the stated peak maps to column ${expected.toFixed(1)} ± ${tolPx.toFixed(1)}`);
  assert.ok(Math.abs(s.peakCol - expected) <= tolPx,
    `the trace's highest point is at column ${s.peakCol}, but it SAYS its peak is at ` +
    `${(slice.hz / 1e6).toFixed(4)} MHz, which is column ${expected.toFixed(1)} of ${s.w}. ` +
    "The readout and the pixels are describing different things.");
  assert.deepEqual(page.exceptions, [], "uncaught exception while tracing");
});

test("the trace is drawn exactly where data exists and is ABSENT everywhere else — in one frame", async (t) => {
  // The spec's second optimisation is also an honesty rule: draw only where data exists, and let a
  // gap read as a gap. The test for that has to defeat the obvious cheat in both directions — a
  // trace that draws nothing passes "absent where unobserved", and a trace that draws everywhere
  // passes "present where observed" — so both edges are pinned **in the same frame**, against a
  // boundary this test knows independently: the **tuned band the socket's own header declares**.
  //
  // Nothing about tile residency enters this: the slice at the growing edge comes from the delivered
  // row, whose extent is exactly `center_hz ± bandwidth_hz/2`. Outside it there is no current frame
  // at all, and "no current frame" must read as nothing rather than as quiet.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");

  const { rect } = await page.waitForCanvas(".sf-canvas",
    (c) => c.distinct >= 16 && c.dominantShare < 0.97, { timeoutMs: 90000 });
  // A live-frame slice, so the boundary under test is the tuned band and not a tile edge.
  await page.waitFor("the trace to state a live-frame slice",
    `/slice [\\d:]+Z \\(live frame\\) · peak/.test(document.querySelector('.sf-trace')?.textContent ?? "")`,
    { timeoutMs: 60000 });
  await page.frames(3);

  const img = await page.shot(path.join(ART, "app-trace-extent.png"));
  const snap = JSON.parse(await page.eval(SNAPSHOT));
  const s = strip(img, rect);
  const win = windowOf(snap.headline);
  const geom = snap.tap.geom;
  assert.ok(geom && geom.bandwidthHz > 0, "the tap never saw a stream header to take the band from");
  const band = { f0Hz: geom.centerHz - geom.bandwidthHz / 2, f1Hz: geom.centerHz + geom.bandwidthHz / 2 };
  const colOf = (hz) => ((hz - win.f0Hz) / win.spanHz) * s.w;
  const expLo = colOf(band.f0Hz), expHi = colOf(band.f1Hz);

  const drawnAt = s.cols.map((v, i) => (v >= 0 ? i : -1)).filter((i) => i >= 0);
  t.diagnostic(`viewport ${(win.f0Hz / 1e6).toFixed(3)}–${(win.f1Hz / 1e6).toFixed(3)} MHz over ${s.w} px; ` +
    `tuned band ${(band.f0Hz / 1e6).toFixed(3)}–${(band.f1Hz / 1e6).toFixed(3)} MHz = columns ` +
    `${expLo.toFixed(1)}–${expHi.toFixed(1)}`);
  t.diagnostic(`drawn: ${s.drawn}/${s.w} columns, from ${drawnAt[0]} to ${drawnAt[drawnAt.length - 1]}`);

  // PRESENT: the drawn columns start and end at the band's edges. A trace that quietly covered only
  // one tile's worth, or that stopped at the pane's centre, fails here.
  assert.ok(s.drawn > 0, "nothing is drawn at all — the absence below would prove nothing");
  // One pooled column plus a stroke's width of slack at each end.
  const tol = Math.max(8, (2 * s.w) / TRACE_COLUMNS);
  assert.ok(Math.abs(drawnAt[0] - Math.max(0, expLo)) <= tol,
    `the trace starts at column ${drawnAt[0]}; the tuned band starts at ${expLo.toFixed(1)}`);
  assert.ok(Math.abs(drawnAt[drawnAt.length - 1] - Math.min(s.w - 1, expHi)) <= tol,
    `the trace ends at column ${drawnAt[drawnAt.length - 1]}; the tuned band ends at ${expHi.toFixed(1)}`);

  // ABSENT: every column outside the band is empty. This is the half that a floor, a zero, or a line
  // interpolated across the gap would fail — and it is not vacuous, because the columns inside the
  // band were just shown to be drawn.
  const outside = s.cols.map((v, i) => ({ v, i })).filter(({ i }) => i < expLo - tol || i > expHi + tol);
  const lit = outside.filter(({ v }) => v >= 0);
  t.diagnostic(`${outside.length} columns lie outside the tuned band; ${lit.length} of them are drawn`);
  assert.equal(lit.length, 0,
    `${lit.length} columns outside the tuned band carry a trace sample (first at ${lit[0]?.i}). ` +
    "There is no current frame out there: drawing one claims a measurement nobody took.");
  // The whole viewport is not the band, or the two assertions above are the same assertion.
  assert.ok(outside.length > s.w * 0.05,
    `only ${outside.length} of ${s.w} columns are outside the band — this frame has no control region`);

  assert.deepEqual(page.exceptions, [], "uncaught exception while measuring the trace's extent");
});

test("a drag that STARTS IN THE TRACE STRIP pans the pane — the strip is a readout, not a hole", async (t) => {
  // The T-457 × T-458 merge break, in the tier that would have caught it end to end. The strip is
  // carved off the top of the pane's rectangle; before the fix, `paneAt` walked only the drawn pane
  // rects, so a pointer down in the strip resolved to no pane and `input.ts` dropped the gesture —
  // not just T-458's region stroke, but plain and alt drags, which T-456 had settled.
  //
  // The rule now: the strip belongs to its pane for every pointer purpose. `ui/test/surface-trace
  // .test.ts` asserts that as a property of a frame; this asserts the whole chain — a real pointer
  // stream, through `input.ts`, into the view — which is the part a pure function cannot speak for.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the trace to draw",
    `/slice [\\d:]+Z/.test(document.querySelector('.sf-trace')?.textContent ?? "")`, { timeoutMs: 90000 });
  const rect = await page.$rect(".sf-canvas");

  // Inside the strip: the top `TRACE_PX` device px of the canvas, at dpr 1.
  const y = rect.y + TRACE_PX / 2;
  assert.ok(TRACE_PX / 2 < rect.h, "the canvas is shorter than the strip — this test is not aimed at it");
  const before = (await page.$text(".sf-chrome")) ?? "";
  await page.drag({ x: rect.x + rect.w * 0.65, y }, { x: rect.x + rect.w * 0.3, y });
  await page.waitFor("the per-viewport readout to change after a drag begun in the strip",
    `(document.querySelector('.sf-chrome')?.textContent ?? "") !== ${JSON.stringify(before)}`,
    { timeoutMs: 15000 });
  const after = (await page.$text(".sf-chrome")) ?? "";
  t.diagnostic(`chrome before: ${before.slice(0, 80)}`);
  t.diagnostic(`chrome after:  ${after.slice(0, 80)}`);

  // T-340's control, unchanged: a pan is a pan. A gesture that began over the trace must be no more
  // able to reach the radio than one that began over the waterfall.
  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|gains|bias_tee|baseband_filter)/.test(r.url));
  assert.deepEqual(control.map((r) => r.url), [], "a drag begun in the trace strip reached the front end");
  assert.deepEqual(page.exceptions, [], "uncaught exception while dragging from the strip");
});

test("a viewport scrubbed into the past traces THAT instant, from the pyramid, and says so", async (t) => {
  // "Pause freezes the view, not the capture": rows keep arriving, so the naive trace keeps drawing
  // the newest one. On a window from a minute ago that line is a spectrum of NOW over a picture of
  // THEN — and it would look entirely convincing. The spec's first addition is exactly this: the
  // trace's time position is the *viewport's*.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });

  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the trace to state a slice",
    `/slice [\\d:]+Z \\([^)]*\\) · peak/.test(document.querySelector('.sf-trace')?.textContent ?? "")`,
    { timeoutMs: 90000 });
  const rect = await page.$rect(".sf-canvas");
  const before = statedSlice((await page.$text(".sf-trace")) ?? "");

  // Freeze the viewport and walk it back through its own window. A drag pans (T-456); the surface
  // clamps at the retained extent, so this cannot run off the end of the record.
  // Upward: the pointer is in GL coordinates (y up) and a pane's time runs up, so dragging toward
  // the top of the screen walks the window BACKWARD. Repeated because one drag is half a window and
  // the window has to clear the live row entirely.
  await page.click(`document.querySelector('.sf-live')`);
  for (let i = 0; i < 4; i++) {
    await page.drag(
      { x: rect.x + rect.w * 0.5, y: rect.y + rect.h * 0.75 },
      { x: rect.x + rect.w * 0.5, y: rect.y + rect.h * 0.25 });
    await page.frames(3);
    const now = (await page.$text(".sf-trace")) ?? "";
    if (/slice [\d:]+Z \(\d/.test(now)) break;   // the source is a cell, not the live frame
  }
  // Belt and braces: even with the window clamped at the oldest retained instant, capture keeps
  // advancing past a HELD view, so the live row leaves the frozen cell on its own.
  await page.waitFor("the held viewport to stop being drawn from the current frame",
    `/slice [\\d:]+Z \\(\\d/.test(document.querySelector('.sf-trace')?.textContent ?? "")`,
    { timeoutMs: 30000 });
  await page.frames(3);

  const snap = JSON.parse(await page.eval(SNAPSHOT));
  t.diagnostic(`before: ${before.at}Z (${before.source})`);
  t.diagnostic(`scrubbed: ${snap.trace}`);
  // Parsed loosely on purpose: a frozen window may sit where no tile is in hand, and then there is
  // no peak to state. What must hold either way is the *instant* and the *source*.
  const m = /slice ([\d:]+)Z \(([^)]+)\)/.exec(snap.trace);
  assert.ok(m, `the trace stated no slice at all: ${JSON.stringify(snap.trace)}`);
  const after = { at: m[1], source: m[2] };

  // **The claim, stated against the socket rather than against the earlier reading.** Capture never
  // stopped, so rows kept arriving while the view was held; the newest of them is `now`, and the
  // trace must NOT be showing it. Comparing to the pre-scrub reading would be the weaker test — it
  // could pass merely because a second ticked over — so the comparison is with the live row the tap
  // saw at this same moment.
  const newest = snap.tap.recent[snap.tap.recent.length - 1];
  assert.ok(newest, "the tap saw no rows, so 'capture never stopped' is not established");
  const nowZ = new Date(newest.tS * 1000).toISOString().slice(11, 19);
  t.diagnostic(`the socket's newest row at this moment: ${nowZ}Z; the trace is tracing ${after.at}Z`);
  assert.notEqual(after.at, nowZ,
    `the held viewport is tracing ${after.at}Z and the newest delivered row is ${nowZ}Z — the same ` +
    "instant. A frozen view drawing the current frame is a spectrum of now over a picture of then.");
  // The source must be the pyramid, stated as the cell it really is. Claiming the live frame here
  // would be the whole defect; claiming a *cell* is an *instant* would be the quieter version of it.
  assert.notEqual(after.source, "live frame",
    "a viewport in the past is still being drawn from the current frame");
  assert.match(after.source, /\d.* (ms|s|min) cell/,
    `the source is "${after.source}" — a slice from the pyramid must say the cell duration it folds, ` +
    "so a max over a second is not passed off as an instant");
  assert.deepEqual(page.exceptions, [], "uncaught exception while scrubbing");
});
