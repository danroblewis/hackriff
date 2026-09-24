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
//  2. **The render path.** The composited pixels agree with that statement: the highest ink in the
//     drawn trace is in the screen column the stated peak frequency maps to, through the window the
//     pane itself says it is showing. This is the half a correct readout over a broken renderer would
//     fail. **It is asserted of ONE frame** — the readout and the framebuffer are captured with the
//     stream held and the reading bracketed on both sides of the screenshot (T-487), because a
//     readout and a screenshot a round trip apart are two different spectra, and "do two successive
//     frames agree" is not the question this claim is about.
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
import { Browser, tileAsks, waitWhileWorking } from "./harness.mjs";
import { UI_DIR } from "./backend.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;
const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** Height of the trace strip, device px — `TRACE_PX` in ui/src/app/centre/surface.ts. */
const TRACE_PX = 96;
/** `TRACE_COLUMNS` in ui/src/surface/trace.ts. */
const TRACE_COLUMNS = 256;
/** `GLOW_PX` in ui/src/surface/trace.ts: the bloom's stroke width, which also hugs the line. */
const GLOW_PX = 7;

/**
 * The tap. Observes the wire, and — only when a check asks it to — **holds** the page's view of it.
 *
 * `class ... extends WebSocket` rather than a wrapping function, so `new`, the prototype chain and
 * every property the app sets (`binaryType`, `onmessage`) behave exactly as they would.
 *
 * ## The hold, and why a test needs one (T-487)
 *
 * The default is pure observation: it forwards nothing, changes nothing, and answers no question the
 * page asks. `hold()` is the one exception, and it exists because two of the claims below are about
 * **one frame** — what the readout says and what the pixels show — while a CDP client can only read
 * the DOM and the framebuffer in separate round trips. Measured on this fixture: over 40 unheld
 * brackets the readout changed between the two reads **38 times**, its stated peak wandering across
 * ~15 screen columns from frame to frame as an FM signal's instantaneous peak bin moves. A readout
 * and a screenshot taken a round trip apart are therefore two different spectra, and comparing them
 * is the adjacent-question mistake: it asks whether two *successive* frames agree, not whether one
 * frame's readout describes its own pixels.
 *
 * So the hold stops delivery to the page's own `onmessage` — the tap's listener is registered in the
 * constructor, before `ui/src/app/net.ts` assigns one, so `stopImmediatePropagation` is enough and no
 * property surgery is needed. The app's live row, and with it the pane's live edge, then stand still,
 * and every subsequent frame draws the same slice from the same delivered row through exactly the
 * product code under test. Nothing about the render path changes; only the arrival of the *next*
 * frame is deferred, which is a state a stalled network reaches anyway.
 *
 * The tap keeps recording while held, and counts what it withheld, so "rows kept arriving and the
 * picture did not move" is a measured claim rather than an assumption — and so a hold that silently
 * did nothing (because the socket had gone quiet on its own) is distinguishable from one that worked.
 */
const TAP = `(() => {
  const Base = WebSocket;
  const tap = { headers: 0, rows: 0, geom: null, recent: [], held: false, withheld: 0 };
  tap.hold = () => { tap.held = true; tap.withheld = 0; };
  tap.resume = () => { tap.held = false; };
  window.__hkTap = tap;
  const observe = (e) => {
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
  };
  class TapSocket extends Base {
    constructor(...a) {
      super(...a);
      // Registered HERE, in the constructor, so it runs before the handler \`net.ts\` assigns later —
      // which is what lets the hold below withhold delivery without touching the socket or the app.
      this.addEventListener("message", (e) => {
        try { observe(e); } finally {
          if (tap.held) { tap.withheld++; e.stopImmediatePropagation(); }
        }
      });
    }
  }
  window.WebSocket = TapSocket;
})();`;

/** What the page is saying and what the socket delivered, read in ONE evaluation so they agree. */
const SNAPSHOT = `(() => {
  const row = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
  const canvas = document.querySelector('.sf-canvas');
  const box = canvas ? canvas.getBoundingClientRect() : null;
  return JSON.stringify({
    trace: document.querySelector('.sf-trace')?.textContent ?? "",
    headline: row ? row.children[1].textContent : "",
    // **The rectangle every pixel in this observation is indexed by, read in the SAME evaluation as
    // the words** — see [[heldObservation]] for what a stale one costs.
    rect: box ? { x: box.x, y: box.y, w: box.width, h: box.height } : null,
    // PaneReport, as the pane itself states it: N tiles - N coarse stand-ins - N pending. What the
    // renderer actually drew this frame WITH; see isResident below.
    counts: row?.querySelector('.hk-surface-counts')?.textContent ?? "",
    tap: { headers: window.__hkTap.headers, rows: window.__hkTap.rows, geom: window.__hkTap.geom,
           held: window.__hkTap.held, withheld: window.__hkTap.withheld,
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
const STATED_PEAK = /slice ([\d:]+)Z \(([^)]+)\) · peak (-?[\d.]+) dB at ([\d.]+) MHz/;

/** `slice 12:34:56Z (live frame) · peak -41.2 dB at 100.3021 MHz · max-hold …` */
function statedSlice(trace) {
  const m = STATED_PEAK.exec(trace);
  assert.ok(m, `the slice stated no peak: ${JSON.stringify(trace)}`);
  return { at: m[1], source: m[2], db: Number(m[3]), hz: Number(m[4]) * 1e6 };
}

/**
 * **The first readout that satisfies `re`, returned by the read that satisfied it** — and why a
 * `waitFor` followed by a `$text` is not that.
 *
 * `page.waitFor` answers a *boolean* and returns how long it took, so a caller that wants the text
 * has to read the element again, one or more round trips later. The readout it gets back is then a
 * **different frame**, and nothing makes that frame still satisfy the property that was waited for.
 *
 * Here the gap is not hypothetical, and it is not a flake in the product either. Two states of this
 * readout both match "a slice with a peak" and they are reached by different routes:
 *
 *  - the **live frame** is a delivered row, so it states a peak with no tile resident at all;
 *  - a **cell** is the pyramid's answer for the pane's own time position, so it states a peak only
 *    once a tile covering that cell has arrived.
 *
 * A cold page reaches the first within ~200 ms and the second only when the tile route answers, and
 * the pane's time position crosses between them as the live row and the polled edge move relative to
 * one another. Measured on this fixture, over 8 fresh loads on this tree and 8 on `main`, the
 * readout drops to a peak-less cell — `no tile in hand for this span yet (N pending, …)` — within a
 * few hundred ms of the first peak on **2 to 3 loads in 8, identically on both trees**; with the
 * renderer CPU-throttled the pre-scrub read below lands inside that window outright. So a wait for
 * "a slice with a peak" followed by a re-read is a wait for one state and an assertion about
 * another, which is the T-487 adjacent-question mistake spelled with time instead of with pixels.
 *
 * The bound is stated and is not a wall-clock guess about the product: it polls at `everyMs` until
 * `timeoutMs`, and it returns the matching text itself, so the string asserted on is the string the
 * property was checked against.
 */
async function traceMatching(page, what, re, { timeoutMs = 90000, everyMs = 100 } = {}) {
  const t0 = Date.now();
  for (;;) {
    const text = (await page.$text(".sf-trace")) ?? "";
    if (re.test(text)) return text;
    if (Date.now() - t0 > timeoutMs) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}\n  last readout: ` +
        `${JSON.stringify(text)}\n  exceptions: ${JSON.stringify(page.exceptions.slice(0, 3))}`);
    }
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/**
 * Where the trace was drawn in the strip, and **which series each pixel belongs to** (T-475).
 *
 * The strip is the top `TRACE_PX` device px of the canvas — the pane is that much shorter and the map
 * is along the bottom — and four things are drawn in it over `BACKDROP` (rgb 10,10,13): the current
 * slice, its bloom, the afterglow rows, and the max-hold. T-457's separator (`b > r` is the slice)
 * died with the plain line: the slice now carries the **waterfall's ramp**, so its peak is yellow or
 * white and its floor is blue, and a single channel comparison says nothing about which series a
 * pixel is.
 *
 * The separator that replaces it needs no second copy of the ramp and no test-only flag, because the
 * product code makes it structural (see `TraceStyle.shade` in ui/src/surface/trace.ts): **exactly one
 * line on the strip is drawn from the ramp.** So
 *
 *  - **the afterglow and the bloom are neutral grey** — achromatic, `max − min ≈ 0`;
 *  - **the max-hold is magenta**, which is off the ramp entirely: no point on the ramp has red
 *    *and* blue both above green (`ui/test/surface-trace.test.ts` asserts that of every point on
 *    the ramp, so this classification cannot quietly stop being true);
 *  - **the current slice is everything else that is bright** — chromatic, or near-white at the very
 *    top of the ramp, which no grey here can reach because the mono ramp stops at 0.6.
 *
 * ## The max-hold rule is an ORDERING, not a ratio — and why the ratio was wrong (T-532)
 *
 * The first form of this asked `r > g * 1.3 && b > g * 1.3`, and it was **a ratio applied to
 * anti-aliased pixels**, which is the part that does not hold. A stroke's outermost pixel is a
 * partial blend, the compositor mixes in **linear** light and the framebuffer is **sRGB-encoded**,
 * and that encoding does not preserve channel *ratios* under partial coverage — it compresses the
 * high channels more than the low one, and a shadow row underneath lifts the low channel further.
 * So the magenta line's own feather slides down the ratio scale until one of the two tests fails
 * while the other still passes, and the pixel is then read as the current slice.
 *
 * Measured, on the failures this rewrite came from: the max-hold's top edge came back as
 * `(121, 91, 114)` and `(132, 99, 125)` — `b` under `1.3 g` by four units in each, while `r` cleared
 * it — and the same edge two columns along came back as `(144, 96, 132)`, which passes. Two columns
 * apart, a
 * coin-flip about sub-pixel coverage, and the loser was counted as the highest ink of the *slice*:
 * at row 2 of the strip while the slice itself was drawn at row 16, 12 px lower, where the readout
 * said it was. Under concurrent load **9 of 24 runs** failed that way. Nothing about the product
 * moved; the instrument was reading the wrong line.
 *
 * The property that **does** survive compositing is the ordering. Blending is `a·H + (1-a)·B` per
 * channel, and every other series on this strip is achromatic or near it (the backdrop is
 * `rgb(10,10,13)`, the afterglow and the bloom are grey), so `b - g` and `r - g` keep the sign the
 * max-hold's own ink gives them: `HOLD_INK` is `[1.0, 0.45, 0.85]`, magenta, with blue and red both
 * **above** green. sRGB encoding is monotonic, so the sign survives the 8-bit write too. And the
 * ramp never has that shape: wherever a point on the ramp has `r > g` (its yellow-to-white end) its
 * blue is far *below* green, and wherever blue is above green (its black-to-cyan end) red is below
 * it. `ui/test/surface-trace.test.ts` asserts exactly that of every point on the ramp — in this
 * form, the one this file relies on — so the classification cannot quietly stop being true.
 *
 * `HOLD_MARGIN` is the slack. A hold pixel faint enough to fall under it is also far below
 * `isRampInk`'s chroma floor (magenta holds `b - g ≈ 0.75 (r - g)` across every coverage, so a
 * pixel with `r - g ≥ 30` carries `b - g ≈ 22`), which is what keeps the two rules from meeting in
 * the middle.
 */
const CHROMA = (r, g, b) => Math.max(r, g, b) - Math.min(r, g, b);
/** Well under the `b - g ≈ 22` a pixel has by the time it is bright enough to be ramp ink. */
const HOLD_MARGIN = 8;
const isHoldInk = (r, g, b) => r - g >= HOLD_MARGIN && b - g >= HOLD_MARGIN;
const isRampInk = (r, g, b) => r + g + b >= 120 && (CHROMA(r, g, b) >= 30 || r + g + b >= 620)
  && !isHoldInk(r, g, b);
const isGreyInk = (r, g, b) => r + g + b >= 60 && CHROMA(r, g, b) < 12;
/** What the retired ratio rule called the max-hold. Kept only to count what it used to leak. */
const wasHoldInk = (r, g, b) => r > g * 1.3 && b > g * 1.3 && r + g + b > 150;

function strip(img, rect) {
  const x0 = Math.round(rect.x), y0 = Math.round(rect.y), w = Math.round(rect.w);
  const cols = new Array(w).fill(-1);       // topmost slice pixel per column, -1 = none
  const ink = new Array(w).fill(null);      // and the colour it was drawn in
  let slicePx = 0, holdPx = 0, greyPx = 0, rescued = 0;
  for (let x = 0; x < w; x++) {
    for (let y = 0; y < TRACE_PX; y++) {
      const d = ((y0 + y) * img.width + (x0 + x)) * 4;
      const r = img.data[d], g = img.data[d + 1], b = img.data[d + 2];
      if (isHoldInk(r, g, b)) {
        holdPx++;
        // The pixels the ratio rule used to hand to the slice: max-hold ink whose feather had
        // slid under `1.3 g`. Counted, not just excluded, so this stays a measured claim — a tree
        // where it drops to zero is one where the ordering rule is no longer doing any work.
        if (!wasHoldInk(r, g, b) && r + g + b >= 120 && CHROMA(r, g, b) >= 30) rescued++;
        continue;
      }
      if (isRampInk(r, g, b)) {
        slicePx++;
        if (cols[x] < 0) { cols[x] = y; ink[x] = [r, g, b]; }
      } else if (isGreyInk(r, g, b)) greyPx++;
    }
  }
  let peakCol = -1, peakY = Infinity, lowY = -1;
  for (let x = 0; x < w; x++) {
    if (cols[x] < 0) continue;
    if (cols[x] < peakY) { peakY = cols[x]; peakCol = x; }
    if (cols[x] > lowY) lowY = cols[x];
  }
  return { w, cols, ink, slicePx, holdPx, greyPx, rescued, peakCol, peakY, lowY,
    drawn: cols.filter((v) => v >= 0).length };
}

/**
 * The colours the WATERFALL painted just under the strip, across one **pooled trace column**.
 *
 * Two extents, and both are the honest ones rather than conveniences:
 *
 *  - **Across:** a trace column is `viewport / TRACE_COLUMNS` screen px wide (3.2 px here) and pools
 *    by MAX over every cell in it — at this zoom a frequency cell is about one px, so the colour the
 *    trace draws is the loudest of a handful of cells. Reading a single screen px would be asking
 *    whether the trace drew the cell at its centre, which is not what a max-pool claims.
 *  - **Down:** the slice is the pyramid's row at the pane's own time position, which is the top cell
 *    of the pane. A few rows of pixels covers it at any of the tiers this view resolves to.
 */
function cellColours(img, rect, x, halfPx, rows) {
  const y0 = Math.round(rect.y) + TRACE_PX;
  const out = [];
  for (let dx = -halfPx; dx <= halfPx; dx++) {
    const x0 = Math.round(rect.x) + x + dx;
    if (x0 < Math.round(rect.x) || x0 >= Math.round(rect.x + rect.w)) continue;
    for (let y = 2; y < 2 + rows; y++) {
      const d = ((y0 + y) * img.width + x0) * 4;
      out.push([img.data[d], img.data[d + 1], img.data[d + 2]]);
    }
  }
  return out;
}

/**
 * **The colour of the stroke's CORE**, not of its feathered edge.
 *
 * `tracepass.ts` fades coverage over the last device pixel of the stroke, so the topmost drawn pixel
 * of a line is a partial blend with the backdrop — a real property of an anti-aliased line and
 * exactly the wrong pixel to compare against a cell. The core is the fullest-coverage RAMP pixel
 * within a stroke of the top; the ramp-ink filter matters because an afterglow row can cross the
 * slice, and grey at the top of the ramp's scale is brighter than the ramp's own dark end, so
 * "the brightest pixel in the stroke" alone would sometimes return a shadow.
 */
function coreInk(img, rect, x, top) {
  const x0 = Math.round(rect.x) + x, y0 = Math.round(rect.y);
  let best = null, bestSum = -1;
  for (let y = top; y <= top + 3 && y < TRACE_PX; y++) {
    const d = ((y0 + y) * img.width + x0) * 4;
    const c = [img.data[d], img.data[d + 1], img.data[d + 2]];
    if (!isRampInk(c[0], c[1], c[2])) continue;
    const sum = c[0] + c[1] + c[2];
    if (sum > bestSum) { bestSum = sum; best = c; }
  }
  return best ?? [0, 0, 0];
}

/** Closest match, as a max-channel distance, between one colour and a set of them. */
function nearestDist(c, set) {
  let best = Infinity;
  for (const o of set) {
    best = Math.min(best, Math.max(Math.abs(c[0] - o[0]), Math.abs(c[1] - o[1]), Math.abs(c[2] - o[2])));
  }
  return best;
}

/**
 * The topmost drawn pixel row **within one pooled trace column**, `-1` if that column drew nothing.
 *
 * Why a span and not `cols[Math.round(centrePx)]`: the readout states a *frequency*, and the
 * frequency it states is the centre of a pooled column (`peakOf`: `f0 + (at + 0.5) * colHz`). One
 * such column is `w / TRACE_COLUMNS` screen pixels wide — 3.2 px at this viewport — so the honest
 * question the pixels can answer is "what is drawn in that column", not "what is drawn in the single
 * pixel its centre happens to round to", which lands in the *next* column whenever the centre falls
 * on a half-pixel.
 */
function topWithin(cols, centrePx, widthPx) {
  const a = Math.max(0, Math.floor(centrePx - widthPx / 2));
  const b = Math.min(cols.length - 1, Math.ceil(centrePx + widthPx / 2));
  let top = -1;
  for (let x = a; x <= b; x++) if (cols[x] >= 0 && (top < 0 || cols[x] < top)) top = cols[x];
  return { top, a, b };
}

/**
 * **The state both frame-accurate checks below are about**, in the two forms they need it: an
 * in-page expression to wait on, and a predicate over a readout already read back.
 *
 * One definition, because the two must agree. A wait that establishes one state and an assertion
 * that accepts another is the whole defect this file was rewritten for.
 */
/**
 * **The state both T-475 checks need, in the two forms `heldObservation` wants** — one definition,
 * for the same reason `LIVE_FRAME_EXPR` is one: a wait that establishes one state and an assertion
 * that accepts another is the defect this file was rewritten for.
 *
 * Three conditions, all read inside the page off one DOM read and one tap read so there is no round
 * trip between them:
 *
 *  - **the slice comes from a pyramid cell**, not the live row — which is what makes the trace and
 *    the top row of the waterfall two renderings of *the same cells*;
 *  - **it has a peak to state**, i.e. a tile is actually in hand. A frozen window can sit where none
 *    is, and then the strip is empty: a true statement about a frame these checks are not about
 *    (T-487's lesson, applied to one more precondition);
 *  - **its instant is at least `lagS` behind the newest row the socket delivered**, so "this viewport
 *    is in the past" is legible at the second resolution the readout prints.
 *
 * Getting there needs no drag: "pause freezes the view, not the capture", so pressing Live and
 * waiting *is* capture walking away from a held window. That is also why this does not depend on how
 * far one pan happens to travel — which is the thing that has moved twice under this file already.
 */
const CELL_SLICE_RE = /slice [\d:]+Z \(\d[^)]*(ms|s|min) cell\) · peak/;
/**
 * **The page saying the rows before its instant are NOT in hand** (`afterglowAbsence`): the tiles
 * the afterglow would be read from are pending, drawn by a coarse stand-in, refused, or answered only
 * up to an earlier instant. A frame in that state says nothing about the afterglow either way — it
 * is the not-loaded state, not the claim.
 */
const AFTERGLOW_NOT_IN_HAND_RE = /afterglow — the rows before this instant are not all in hand/;
/**
 * **The pane drew this frame with the tiles it is addressing, and nothing standing in for them.**
 *
 * A coarse stand-in is a real measurement — the parent level's, max-held over a cell several times
 * larger — stretched across a child's place while the child is in flight. It is drawn honestly
 * (`uFallback`), and it is a *different dB* from the one the trace read at the fine level. So a
 * pixel comparison made while one is on screen is not about the ramp at all: `surface-colour.e2e
 * .mjs` measured a stand-in swap moving 5 038 px of an untouched pane, and calls blaming that on
 * the ramp "the adjacent-question error this file is written against". `live-edge` and
 * `canvas-journey` both gate their pixel claims on this same readout; the colour check below was
 * the one that did not, and its failure message accordingly named T-397 for a state T-397 is not.
 *
 * `tiles > 0` as well, because `0 tiles · 0 coarse stand-ins · 0 pending` is a pane that addressed
 * nothing — a true statement about a frame this claim is not about.
 */
const COUNTS_RE = /(\d+) tiles · (\d+) coarse stand-ins? · (\d+) pending/;
const isResident = (counts) => {
  const m = COUNTS_RE.exec(counts ?? "");
  return !!m && Number(m[1]) > 0 && Number(m[2]) === 0 && Number(m[3]) === 0;
};
const scrubbedExpr = (lagS, { resident = false, afterglowInHand = false } = {}) => `(() => {
  const txt = document.querySelector('.sf-trace')?.textContent ?? "";
  const m = /slice (\\d\\d):(\\d\\d):(\\d\\d)Z \\(\\d[^)]*(ms|s|min) cell\\) · peak/.exec(txt);
  const r = window.__hkTap?.recent?.[window.__hkTap.recent.length - 1];
  if (!m || !r) return false;
  // [[AFTERGLOW_NOT_IN_HAND_RE]], in the page, for the same one-rule-two-places reason as below.
  if (${afterglowInHand} && /afterglow — the rows before this instant are not all in hand/.test(txt)) return false;
  if (${resident}) {
    // The same condition as isResident(), in the page, so the wait establishes exactly the state
    // the accept predicate re-verifies. Two spellings of one rule is the defect this file was
    // rewritten for; this is one rule in the two places the harness needs it.
    const row = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
    const c = /(\\d+) tiles · (\\d+) coarse stand-ins? · (\\d+) pending/
      .exec(row?.querySelector('.hk-surface-counts')?.textContent ?? "");
    if (!c || Number(c[1]) === 0 || Number(c[2]) !== 0 || Number(c[3]) !== 0) return false;
  }
  const at = Number(m[1]) * 3600 + Number(m[2]) * 60 + Number(m[3]);
  const now = new Date(r.tS * 1000);
  const nowS = now.getUTCHours() * 3600 + now.getUTCMinutes() * 60 + now.getUTCSeconds();
  return nowS - at >= ${lagS};
})()`;

/**
 * **Wait for a scrubbed pane's state for as long as the page is WORKING towards it — not for a
 * number of seconds** (the deflake, 2026-09-24).
 *
 * Both states the scrubbed checks need — "a cell slice with a peak", and "every tile the pane
 * addresses resident" — are reached by the tile route answering, and nothing else. Their waits
 * were wall-clock deadlines (12 s a park attempt, 20 s for residency): a bet on that route's
 * service rate, which this repo measured moving more than twenty-fold with load (harness.mjs,
 * [[waitWhileWorking]]). The gate's signature, three times in two days: `timed out after 20000 ms
 * waiting for the trace to be drawn from a pyramid cell … (no coarse stand-ins, nothing pending)`,
 * green alone in ~31 s each time. Measured on this file at load 17–20: one tile request on the
 * wire at a time, ~4 s each, and a frozen pane reaching `8 tiles · 0 coarse stand-ins · 0 pending`
 * 8 s after the park; a little more contention and both scrubbed checks went red on runs ALONE —
 * the colour check still filling at 20 s, and the afterglow check's eight park attempts each
 * timing out at 12 s on a readout that still said `no tile in hand for this span yet (24 pending)`.
 * Holding every tile response 13 s (CDP `Fetch`, so the request stays open exactly as a slow
 * route's does) reproduces the park failure deterministically on the old waits.
 *
 * So the wait runs while the pane's report (its tile counts and the trace's own words) changes or
 * a tile request is on the wire — including one the route is still answering — and gives up when
 * both have been still for [[waitWhileWorking]]'s `stallMs`. The state asserted is unchanged; a pane
 * that stops working without reaching it (a hole in the history, a refusal made terminal, T-523's
 * wedge) is reported as before, with the last report and what the wire did.
 */
const PANE_REPORT = (expr) => `JSON.stringify({
  ok: ${expr},
  counts: document.querySelector('.hk-surface-viewport[data-viewport="pane"]')
    ?.querySelector('.hk-surface-counts')?.textContent ?? "",
  trace: document.querySelector('.sf-trace')?.textContent ?? "",
})`;
async function whileTilesArrive(page, what, expr) {
  const n0 = page.requests.length;
  const r = await waitWhileWorking(page, async () => JSON.parse(await page.eval(PANE_REPORT(expr))),
    (v) => v.ok === true, { openIsWork: true });
  const tiles = page.requests.slice(n0).filter((q) => q.url.includes("/api/tiles"));
  await page.settleBodies();
  const answers = {};
  for (const a of tileAsks(tiles)) answers[a.status ?? "unanswered"] = (answers[a.status ?? "unanswered"] ?? 0) + 1;
  const wire = `${tiles.length} tile request(s), ${tiles.filter((q) => q.endedMs !== null).length} ended; ` +
    `addresses by answer ${JSON.stringify(answers)}`;
  if (!r.ok) {
    throw new Error(`the page stopped working without reaching ${what}: after ${r.ms} ms, nothing ` +
      `changed for ${r.stalledMs} ms (${wire})\n  pane: ${JSON.stringify(r.value.counts)}\n` +
      `  trace: ${JSON.stringify(r.value.trace)}\n  exceptions: ${JSON.stringify(page.exceptions.slice(0, 3))}`);
  }
  return { ms: r.ms, wire };
}

/**
 * **Park a viewport on an observed pyramid cell behind the live edge** — by RE-ESTABLISHING the
 * state, never by waiting for it.
 *
 * Pressing Live once and waiting looks like it should work and does not, and the way it fails is
 * worth writing down because it is the same shape as T-487's. A viewport freezes on the instant it
 * was following, which is the cell the pipeline is *still writing*; if the pyramid never ends up with
 * an observed cell there, the frozen window sits over a hole **for ever**, and every extra second of
 * waiting only widens the gap to the live edge without changing the cell being asked about. Measured:
 * a 90 s wait ended with the viewport 86 s in the past reading `nothing observed across this span`,
 * while the identical predicate in the test below was satisfied in 3 s — the difference being luck
 * about which second each one happened to freeze on.
 *
 * So each attempt returns to the growing edge, waits for a live frame that HAS a peak (data is
 * arriving), freezes there, and waits while the pyramid is answering for that cell
 * ([[whileTilesArrive]] — as long as tiles are arriving, never a fixed number of seconds). A failed
 * attempt — the pane went still without a cell — freezes somewhere else rather than waiting longer
 * in the same hole.
 */
async function scrubOntoCell(page, lagS, tries = 8) {
  await page.waitFor("the spectrum socket to deliver rows the tap can see",
    "(window.__hkTap?.rows ?? 0) > 3 && !!window.__hkTap.geom", { timeoutMs: 60000 });
  let last = "";
  for (let i = 0; i < tries; i++) {
    // Back to the growing edge. `.sf-live` toggles, so this presses until the pane says it is
    // following rather than assuming one press means one direction.
    //
    // **`data-following`, not the trace's source label** — T-478's standing rule in this suite, and
    // T-501 is why it now matters here as well as in `surface-nav`. This used to press until the
    // readout said `(live frame)`, which is a statement about which SOURCE answered the slice, not
    // about where the pane is. At the old 1 s floor the socket's newest row always fell inside the
    // cell at the pane's time position, so the two coincided; at the display floor the cell is
    // 80 ms and the pane's live edge lags the socket by more than one of them, so a pane that is
    // genuinely following is answered from the pyramid and `(live frame)` is a transient state this
    // loop could wait out its whole timeout for. Measured: 0 of 6 attempts reached it, while the
    // chrome said `LIVE` throughout. What this helper needs is the pane back at the growing edge,
    // and that is a fact the chrome states.
    for (let k = 0; k < 3; k++) {
      // `page.eval` returns the VALUE, not its string form — comparing against "true" here silently
      // clicked three times every attempt and left the viewport frozen.
      if ((await page.eval(FOLLOWING_EXPR)) === true) break;
      await page.click(`document.querySelector('.sf-live')`);
      await page.frames(8);
    }
    await page.waitFor("the pane to be back at the growing edge", FOLLOWING_EXPR,
      { timeoutMs: 30000 });
    await page.click(`document.querySelector('.sf-live')`);
    try {
      const w = await whileTilesArrive(page, `a pyramid-cell slice at least ${lagS} s behind the live edge`,
        scrubbedExpr(lagS));
      return `attempt ${i + 1} (the cell arrived ${w.ms} ms after the freeze; ${w.wire})`;
    } catch (e) {
      last = String((e && e.message) || e);
      await page.frames(4);
    }
  }
  throw new Error("could not park a viewport on an OBSERVED pyramid cell behind the live edge after " +
    `${tries} attempts — the last freeze landed somewhere the history has no cell: ${last}`);
}

/** The pane is at the growing edge — the chrome's own fact, never the readout string (T-478). */
const FOLLOWING_EXPR =
  `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"][data-following="true"]').length > 0`;

const LIVE_FRAME_EXPR =
  `/slice [\\d:]+Z \\(live frame\\) · peak/.test(document.querySelector('.sf-trace')?.textContent ?? "")`;
const isLiveFrame = (snap) => /slice [\d:]+Z \(live frame\) · peak/.test(snap.trace);

/**
 * **One observation: the readout and the composited pixels of the SAME frame, in a KNOWN state**
 * (T-487).
 *
 * A CDP client reads the DOM with one round trip and the framebuffer with another, so the naive
 * sequence `waitFor(state); shot(); eval()` proves nothing about one frame. Two ways it comes apart
 * were measured on this fixture, and this one function closes both:
 *
 *  - **The picture moves between the reads.** Over 40 unheld brackets the readout changed 38 times,
 *    its stated peak wandering ~15 screen columns as the FM signal's instantaneous peak bin moves.
 *    So the stream is **held** (see `TAP`) and the screenshot is **bracketed** by the readout on both
 *    sides, which must come back identical.
 *  - **The state the wait established has lapsed by the screenshot.** Measured over 14 fresh page
 *    loads: once, the slice's source had flipped from the live row to a pyramid cell with no tile in
 *    hand, so the strip was empty and the check read it as "the trace drew nothing". So the state is
 *    **re-established and then re-verified on the observed readout** rather than assumed to persist.
 *
 *  - **THE RECTANGLE THE PIXELS ARE INDEXED BY HAS MOVED** (the deflake, 2026-09-23). Every caller
 *    used to read the canvas's box once, at `waitForCanvas`, and index this observation's framebuffer
 *    with it — across `scrubOntoCell`'s up-to-eight press-and-wait attempts and this function's own
 *    six, which is tens of seconds. The chrome above the stage is not a fixed height over that: the
 *    trace readout gains and loses clauses ("no tile in hand for this span yet (4 pending…)",
 *    "max-hold over 18.5 s", "afterglow …"), the viewport row's level cell wraps and un-wraps (T-505),
 *    and pressing Live swaps `LIVE` for an offset — so the canvas slides, and a stale `rect.y` reads
 *    the page ABOVE the pane. `strip()` still finds the stroke (its 96 px window overlaps the real
 *    strip either way) while `cellColours()`, which is `rect.y + TRACE_PX + 2`, lands off the pane
 *    entirely and returns the backdrop `rgb(10,10,13)` for every column. That is exactly what the
 *    gate's pooled tier reported: *"only 0.0% of trace columns are painted a colour the waterfall
 *    paints at the same frequency … 1 run(s), widest 586 column(s) = 100.0% of the drawn span"*, with
 *    the first misses showing cyan trace ink against eight identical `[10,10,13]` cells, and the pane
 *    itself reporting `24 tiles · 0 coarse stand-ins · 0 pending` — a fully drawn pane, measured
 *    somewhere else. Green alone, 6/6. So the box is read in the same evaluation as the words, is
 *    bracketed by the screenshot exactly as they are, and comes back with the image; `obs.rect` is
 *    what every pixel read in this file uses. `fog-of-war.e2e.mjs` carries the same note against the
 *    same mistake, one file along.
 *
 * Each attempt therefore resumes the stream, waits for the state, holds, and observes; an attempt
 * that fails any of these tests is retried from the top rather than accepted. Exhausting them is a
 * failure, never a skip: it would mean the page cannot be held in the state the claims are about.
 */
const sameBox = (a, b) => !!a && !!b && a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;

async function heldObservation(page, shotPath, { expr, accept, what, tries = 6, timeoutMs = 60000, scrubbed = false }) {
  let last = null;
  for (let i = 0; i < tries; i++) {
    await page.eval("window.__hkTap.resume()");
    // A scrubbed pane's state is reached by tiles arriving, so it is waited for while they arrive
    // ([[whileTilesArrive]]); a live frame needs no tile and keeps its plain bound.
    const waited = scrubbed ? await whileTilesArrive(page, what, expr)
      : { ms: await page.waitFor(what, expr, { timeoutMs }), wire: "" };
    await page.eval("window.__hkTap.hold()");
    await page.frames(4);
    const before = JSON.parse(await page.eval(SNAPSHOT));
    const img = await page.shot(shotPath);
    const after = JSON.parse(await page.eval(SNAPSHOT));
    const still = before.trace === after.trace && before.headline === after.headline
      && sameBox(before.rect, after.rect);
    if (still && accept(before)) {
      return { snap: before, img, rect: before.rect, withheld: after.tap.withheld, waited,
        rowsDuring: after.tap.rows - before.tap.rows, tries: i + 1 };
    }
    last = { still, accepted: accept(before), before: before.trace, after: after.trace,
      held: after.tap.held, box: sameBox(before.rect, after.rect),
      rects: `${JSON.stringify(before.rect)} -> ${JSON.stringify(after.rect)}` };
    await page.frames(2);
  }
  throw new Error(
    `the page would not hold still in the state under test across ${tries} attempts, so the readout ` +
    "and the pixels cannot be compared as one frame.\n" +
    `  waiting for: ${what}\n  held: ${last?.held}; readout unchanged across the capture: ${last?.still}; ` +
    `canvas box unchanged across the capture: ${last?.box} (${last?.rects})\n` +
    `  state still held at the capture: ${last?.accepted}\n  before: ${last?.before}\n  after:  ${last?.after}`);
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
  // The source the readout NAMES has to be the live frame. The pane's time position is its own
  // window's top, and only there is the delivered row finer than a cell — so this is the state in
  // which the two things being compared are the same thing. Everywhere else the slice legitimately
  // comes from the pyramid, and comparing it to a stream row would be the adjacent-question mistake.
  //
  // **Everything from here to the end of the render check describes a single frame** (T-487): the
  // app's live row is held, so the words and the picture are two readings of the same spectrum
  // rather than of two successive ones. Capture itself never stops — `withheld` below counts the
  // rows that arrived on the wire while the picture stood still, which is what makes this a held
  // *view* rather than a quiet socket.
  const obs = await heldObservation(page, path.join(ART, "app-trace-strip.png"), {
    what: "the trace to state a live-frame slice", expr: LIVE_FRAME_EXPR, accept: isLiveFrame,
  });
  const snap = obs.snap;
  t.diagnostic(`trace readout: ${snap.trace}`);
  t.diagnostic(`tap: ${snap.tap.headers} headers, ${snap.tap.rows} rows, ${snap.tap.recent.length} retained`);
  t.diagnostic(`one observation on attempt ${obs.tries}: ${obs.rowsDuring} rows arrived on the wire ` +
    `during the bracket (${obs.withheld} withheld from the page since the hold) and the readout did not move`);
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
  // T-470 changed *which* measurement this is, so the wording it asserts changed with it. It used to
  // read "measured from the served tiles", which described a range tracked from whatever was on
  // screen — the defect. The claim the assertion is actually making is unchanged and is the one that
  // matters: the trace states the range it is drawn against, **and says how that range was decided**,
  // so a surprising picture is diagnosable rather than mysterious.
  assert.match(snap.trace,
    /scale -?[\d.]+ dB … -?[\d.]+ dB, (measured over the region and anchored there|measured from the tiles on screen \(auto-contrast\))/,
    "the trace must say which measured range it is drawn against, and how that range was decided");

  // ---- (2) the render path: the pixels agree with the statement, IN THE SAME FRAME ----
  const s = strip(obs.img, obs.rect);
  assert.ok(s.slicePx > 20, `the slice series drew ${s.slicePx} pixels in the strip — that is not a trace`);
  // The max-hold's pixels are DIAGNOSTIC, not asserted. Whether it draws depends on a tile being
  // resident for this pane's window, which is a claim about when the pyramid materialises a node —
  // not about the trace, and exactly the thing the incremental-tile work is about to change. Its
  // arithmetic is pinned in `ui/test/surface-trace.test.ts`, where the residency is the fixture.
  t.diagnostic(`strip ink: ${s.slicePx} slice px, ${s.holdPx} max-hold px ` +
    `(${s.rescued} of them bright, chromatic max-hold feather the retired ratio rule handed to the ` +
    "slice — T-532; this is the count that used to decide the assertion below)");

  // **The claim: the highest ink in the strip is in the very column the readout names.**
  //
  // Stated that way round, rather than as "the argmax column is within N px of the stated one",
  // deliberately (T-487). `strip` reduces a whole trace to one `peakCol` by taking the topmost pixel
  // and breaking ties leftward — and a real spectrum's peak is a *plateau*, not a spike: one pooled
  // column is 3.2 px wide here, and measured over 30 held frames the top pixel row was shared by
  // between 3 and 19 columns, in clusters up to 14 px apart. So `peakCol` is an arbitrary pick from a
  // set the framebuffer cannot order, and 8 of those 30 frames had a tied column more than 9.6 px
  // from the stated peak: the old form was sitting on the edge of a coin-flip about which member of
  // the plateau won, which is a property of the tie-break rather than of the trace.
  //
  // Asking whether the STATED column is at the top removes that arbitrariness without softening
  // anything. It is the more exact claim in the direction that matters — the stated column itself,
  // not a ±9.6 px neighbourhood of it — and the only divergence it forgives is one the pixels do not
  // express: another column drawn at the identical height. Measured over the same 30 held frames it
  // held with **zero** slack, top-row equal to top-row, every time.
  const expected = ((slice.hz - win.f0Hz) / win.spanHz) * s.w;
  const colPx = s.w / TRACE_COLUMNS;
  // A stroke is `SLICE_PX` device px across with a pixel of feather either side (`tracepass.ts`), so
  // the top of one column may sit a stroke below another's and still be the same drawn value. That,
  // and nothing else, is the slack. A smooth curve also passes BETWEEN the column centres, so a
  // neighbouring pooled column's ink may ride a fraction of a stroke higher than its own sample.
  const strokePx = 4;
  const { top: topStated, a: loPx, b: hiPx } = topWithin(s.cols, expected, colPx);
  t.diagnostic(`highest drawn sample at column ${s.peakCol} (row ${s.peakY} of ${TRACE_PX}); ` +
    `the stated peak ${(slice.hz / 1e6).toFixed(4)} MHz is pooled column ${expected.toFixed(1)} ` +
    `(px ${loPx}..${hiPx}), drawn at row ${topStated}`);
  assert.ok(topStated >= 0 && topStated <= s.peakY + strokePx,
    `the trace's highest ink is at column ${s.peakCol}, row ${s.peakY}, but the column it SAYS its ` +
    `peak is in — ${(slice.hz / 1e6).toFixed(4)} MHz, px ${loPx}..${hiPx} of ${s.w} — is drawn at ` +
    `row ${topStated < 0 ? "nothing at all" : topStated}. ` +
    "The readout and the pixels are describing different things.");
  // The control that keeps the assertion above from being satisfiable by a flat line: a horizontal
  // trace has every column at the top, so "the stated column is at the top" would say nothing. The
  // strip must have real vertical structure for the claim to be about a peak at all.
  t.diagnostic(`the drawn trace spans rows ${s.peakY}..${s.lowY} of ${TRACE_PX}`);
  assert.ok(s.lowY - s.peakY >= 20,
    `the drawn trace spans only rows ${s.peakY}..${s.lowY} of ${TRACE_PX} — with no vertical ` +
    "structure, 'the stated column is at the top' is true of every column and proves nothing");

  // **Capture never stopped while the view was held**, which is what makes this a held *view* and
  // not a quiet socket — and what keeps "the readout did not move" from being satisfied trivially.
  assert.ok(obs.snap.tap.held, "the hold was not in force for the observation");
  assert.ok(obs.withheld > 0,
    `the socket delivered nothing to withhold while the view was held (${obs.withheld} rows), so the ` +
    "hold proves nothing: the picture may have been still because there was no data, not because it " +
    "was being held");
  await page.eval("window.__hkTap.resume()");
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
  //
  // **"In one frame" is now enforced rather than hoped for** (T-487). The live-frame state this test
  // needs is not one that persists just because a wait once saw it: measured over 14 fresh page
  // loads, one of them had the slice's source flip back to a pyramid cell — with no tile in hand —
  // between the wait and the screenshot, and the strip was empty. Read against the stale precondition
  // that reports as "the trace drew nothing", which is a true statement about a frame this test is
  // not about. So the observation goes through `heldObservation`, which re-establishes the state,
  // holds the stream and re-verifies the state on the readout it actually captured.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");

  // The canvas became a real render. Its BOX is deliberately not kept: the rectangle the pixels
  // below are indexed by comes back with them, from `heldObservation` — see the note there.
  await page.waitForCanvas(".sf-canvas",
    (c) => c.distinct >= 16 && c.dominantShare < 0.97, { timeoutMs: 90000 });
  // A live-frame slice, so the boundary under test is the tuned band and not a tile edge.
  const obs = await heldObservation(page, path.join(ART, "app-trace-extent.png"), {
    what: "the trace to state a live-frame slice", expr: LIVE_FRAME_EXPR, accept: isLiveFrame,
  });
  const img = obs.img;
  const snap = obs.snap;
  const s = strip(img, obs.rect);
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
  t.diagnostic(`one observation on attempt ${obs.tries}: ${obs.rowsDuring} rows arrived on the wire ` +
    `during the bracket (${obs.withheld} withheld from the page since the hold)`);

  // PRESENT: the drawn columns start and end at the band's edges. A trace that quietly covered only
  // one tile's worth, or that stopped at the pane's centre, fails here.
  assert.ok(s.drawn > 0,
    "nothing is drawn at all — the absence below would prove nothing. The readout for this very " +
    `frame says the slice came from the live frame, so there was a row to draw: ${snap.trace}`);
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

  await page.eval("window.__hkTap.resume()");
  assert.deepEqual(page.exceptions, [], "uncaught exception while measuring the trace's extent");
});

test("T-475: the SAME dB is the SAME COLOUR on the trace and in the cells below it", async (t) => {
  // **The ticket's first requirement, read off a real framebuffer.** The unit tier proves the trace
  // asks `cmap` for `(db - lo) / (hi - lo)`; that would still pass if the shader under it used a
  // different ramp, a different range, or a different normalisation — which is exactly the divergence
  // T-397 was (two ramps, one of them stopping at cyan) and exactly what a pixel comparison catches.
  //
  // **What makes the comparison exact.** The claim is about one dB, so the two readings have to be of
  // the same number. A trace drawn from the LIVE ROW is one frame and the cell below it is a max-hold
  // over that cell's whole duration — legitimately different dB, so comparing those would be the
  // adjacent-question mistake this milestone keeps making. On a viewport whose slice comes from the
  // PYRAMID, the slice IS the row of cells at the top of the pane's window (`sliceColumns` is
  // `maxHoldColumns` over a one-cell window), so the trace and the top row of the waterfall are two
  // renderings of *the same cells* — and the colours must match, not merely look similar.
  //
  // **And the pane has to be drawing those cells.** A coarse stand-in is the parent level's dB
  // stretched across a child's place, so a frame containing one is two renderings of *different*
  // cells and says nothing about the ramp. `isResident` is that precondition,
  // established by the wait and re-verified on the observed readout like every other one here.
  // Measured over 81 observations of this check on this fixture: 80 scored 89.1–97.4 % with the
  // pane reporting `N tiles · 0 coarse stand-ins · 0 pending`, and the single failure scored
  // 74.9 % — 147 of 586 columns, which is one of the four tiles this band is cut into, and the one
  // frame whose residency nobody recorded. The residency gate is what makes the next occurrence
  // legible either way; the diagnostics below are the rest of it.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  // As above: the box that indexes the pixels is the observation's own, not this one.
  const opened = await page.waitForCanvas(".sf-canvas",
    (c) => c.distinct >= 16 && c.dominantShare < 0.97, { timeoutMs: 90000 });

  // Freeze the viewport. "Pause freezes the view, not the capture", so the live row keeps arriving
  // and walks out of the frozen top cell on its own — at which point the slice comes from the
  // pyramid and the comparison above is between two readings of one set of cells.
  const parked = await scrubOntoCell(page, 2);
  const obs = await heldObservation(page, path.join(ART, "app-trace-colour.png"), {
    what: "the trace to be drawn from a pyramid cell at least 2 s behind the live edge, with a peak, " +
      "in a pane holding every tile it is addressing (no coarse stand-ins, nothing pending)",
    expr: scrubbedExpr(2, { resident: true }),
    accept: (snap) => CELL_SLICE_RE.test(snap.trace) && isResident(snap.counts),
    scrubbed: true,
  });
  t.diagnostic(`parked on an observed cell on ${parked}; the state under test was reached ` +
    `${obs.waited.ms} ms into the observation (${obs.waited.wire}); the pane drew it with ${obs.snap.counts}`);
  // **How far the canvas moved while this test was getting into state.** Reported rather than
  // asserted: the movement is legitimate (the chrome above the stage grows and shrinks with what it
  // has to say), and the only thing that was ever wrong was measuring pixels with the box from
  // before it. A non-zero number here is this deflake's own evidence.
  const drift = Math.round(obs.rect.y - opened.rect.y);
  t.diagnostic(`the canvas moved ${drift} px vertically and ${Math.round(obs.rect.h - opened.rect.h)} px ` +
    "in height between the first real render and this observation; the pixels below are indexed by " +
    "the box this observation itself reported");
  const s = strip(obs.img, obs.rect);
  t.diagnostic(`readout: ${obs.snap.trace}`);
  t.diagnostic(`strip ink: ${s.slicePx} ramp px, ${s.holdPx} max-hold px, ${s.greyPx} afterglow/bloom px`);
  assert.ok(s.drawn > 20, `only ${s.drawn} of ${s.w} columns carry ramp ink — nothing to compare`);

  // Column by column: the colour the TRACE drew, against the colours the WATERFALL drew in that same
  // screen column, in the top rows of the pane — the cell the slice is a slice of.
  const halfPx = Math.max(1, Math.round(s.w / TRACE_COLUMNS / 2) + 1);
  const ROWS = 6;
  let compared = 0, matched = 0, worst = 0;
  const misses = [];
  // **Where the misses are, not just that there are some.** The two causes this check can see are
  // told apart by their SHAPE, and one number cannot do it: a ramp or range divergence is every
  // column at once, while a place drawn from something other than the fine cells is a CONTIGUOUS
  // BLOCK about as wide as whatever drew it. The one recorded failure was 147 of 586 columns —
  // a quarter of a band this pane cuts into four tiles — and there was no record of which.
  const missAt = [];
  for (let x = 0; x < s.w; x++) {
    if (s.cols[x] < 0) continue;
    compared++;
    const ink = coreInk(obs.img, obs.rect, x, s.cols[x]);
    const cells = cellColours(obs.img, obs.rect, x, halfPx, ROWS);
    const d = nearestDist(ink, cells);
    if (d <= 8) { matched++; worst = Math.max(worst, d); }
    else {
      missAt.push(x);
      if (misses.length < 3) misses.push({ x, d, ink, cells: cells.slice(0, 8) });
    }
  }
  // The misses as runs, and the widest of them — a whole tile's worth is the tell.
  const runs = [];
  for (const x of missAt) {
    const last = runs[runs.length - 1];
    if (last && x === last[1] + 1) last[1] = x; else runs.push([x, x]);
  }
  const widest = runs.reduce((w, [a, b]) => Math.max(w, b - a + 1), 0);
  const shape = `${runs.length} run(s), widest ${widest} column(s) = ${((widest / Math.max(1, compared)) * 100).toFixed(1)}% ` +
    `of the drawn span; drawn with ${obs.snap.counts}`;
  const rate = matched / Math.max(1, compared);
  t.diagnostic(`${s.drawn}/${s.w} columns drawn; ${matched}/${compared} trace columns carry a colour the cells below them also carry ` +
    `(worst matched distance ${worst}/255); misses in ${shape}` +
    `${misses.length ? `; first misses ${JSON.stringify(misses)}` : ""}`);
  assert.ok(rate >= 0.8,
    `only ${(rate * 100).toFixed(1)}% of trace columns are painted a colour the waterfall paints at ` +
    `the same frequency, with the pane reporting fully resident tiles — so the trace and the cells ` +
    `below it are using different ramps or different ranges, which is T-397 in the one place T-475 ` +
    `exists to join up. Misses in ${shape}: one wide block would say some place was drawn from ` +
    `something other than these cells after all; misses spread across every run say the ramp. ` +
    `First misses: ${JSON.stringify(misses)}`);

  // **The control, and it is the whole reason the number above means anything.** Match every column's
  // trace colour against a DISTANT column's cells instead. If a near-match were easy — because the
  // strip is one colour, or the waterfall is — this would score as well as the real comparison, and
  // the test would be measuring the ramp's coarseness rather than the trace's colour.
  let shuffled = 0;
  for (let x = 0; x < s.w; x++) {
    if (s.cols[x] < 0) continue;
    const far = (x + Math.floor(s.w / 3)) % s.w;
    if (nearestDist(coreInk(obs.img, obs.rect, x, s.cols[x]), cellColours(obs.img, obs.rect, far, halfPx, ROWS)) <= 8) shuffled++;
  }
  const shuffledRate = shuffled / Math.max(1, compared);
  t.diagnostic(`negative control: ${shuffled}/${compared} = ${(shuffledRate * 100).toFixed(1)}% match a ` +
    `column a third of the viewport away`);
  assert.ok(rate > shuffledRate + 0.2,
    `the colour match is ${(rate * 100).toFixed(1)}% at the right frequency and ` +
    `${(shuffledRate * 100).toFixed(1)}% at the wrong one — that gap is too small for the match to be ` +
    "about frequency at all, so this comparison proves nothing");
  await page.eval("window.__hkTap.resume()");
  assert.deepEqual(page.exceptions, [], "uncaught exception while comparing trace and cell colours");
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
  const control = page.requests.filter((r) => /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)/.test(r.url));
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
  // The pre-scrub reading, captured by the read that matched it (see `traceMatching`): this line
  // used to wait for a peak and then read the readout again, and the second read is a later frame
  // which need not still have one. It is a *baseline*, not the claim — the claim below is stated
  // against the socket's own newest row, deliberately, and does not consult this at all.
  const before = statedSlice(await traceMatching(page, "the trace to state a slice with a peak",
    STATED_PEAK));
  const rect = await page.$rect(".sf-canvas");

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

test("T-475: the AFTERGLOW is the rows before THIS viewport's instant — demonstrated while SCRUBBED", async (t) => {
  // **The half a client-side persistence buffer cannot do.** "Several fading lines appeared" is
  // satisfied by a ring of whatever frames the page happened to receive — which shows the last few
  // seconds of WALL CLOCK behind a viewport parked in the past, the same defect as a trace pinned to
  // now, one layer down. So this is asserted on a viewport that has left the live edge by a margin
  // the second-resolution readout can express, and the claim is that the glow is back THERE.
  //
  // How the viewport gets there: pausing is enough. "Pause freezes the view, not the capture", so the
  // wait below is capture walking away from a held window — the honest way to produce the state, with
  // no reliance on how far one pan happens to travel.
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: TAP });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitForCanvas(".sf-canvas",
    (c) => c.distinct >= 16 && c.dominantShare < 0.97, { timeoutMs: 90000 });
  const LAG_S = 2;
  const parked = await scrubOntoCell(page, LAG_S);
  // **The rows before the slice must be IN HAND before the afterglow is judged** (the deflake,
  // 2026-09-23). The afterglow is read from what RESIDENT tiles answered, so while the tiles holding
  // the rows before the pane's instant are still pending or drawn by a coarse stand-in, an empty glow
  // is a fact about latency, not about the rows. The gate's pooled tier caught it twice with one
  // signature: the max-hold over the whole 45 s / 2.6 min window peaked at exactly the slice's own dB
  // (−54.1 / −57.4, against −50.0 on every green run) — the slice's tile freshly started and the rest
  // of the window not yet in hand at that level — and the readout said "afterglow — no earlier row in
  // this window". That was the PRODUCT claiming a fact it could not know, and it now says "not all in
  // hand yet" there (`afterglowAbsence`), so a frame in that state is the not-loaded state and not
  // the claim. What varies between runs is the backend's age: alone, a fresh backend's pane spans
  // ~18 s and its afterglow rows share the slice's tile; in the gate the lane's backend had served
  // other files first, the pane spanned minutes, and a freeze near a tile boundary put the rows
  // before the slice in a tile the pane did not yet hold.
  const obs = await heldObservation(page, path.join(ART, "app-trace-afterglow.png"), {
    what: `the viewport to be tracing a pyramid cell at least ${LAG_S} s behind the live edge, with the ` +
      "rows before it in hand",
    expr: scrubbedExpr(LAG_S, { afterglowInHand: true }),
    accept: (snap) => CELL_SLICE_RE.test(snap.trace) && !AFTERGLOW_NOT_IN_HAND_RE.test(snap.trace),
    scrubbed: true,
  });
  t.diagnostic(`parked on an observed cell on ${parked}; the state under test was reached ` +
    `${obs.waited.ms} ms into the observation (${obs.waited.wire}); the pane drew it with ${obs.snap.counts}`);
  const snap = obs.snap;
  const m = /slice ([\d:]+)Z \(([^)]+)\)/.exec(snap.trace);
  assert.ok(m, `the trace stated no slice: ${JSON.stringify(snap.trace)}`);
  const newest = snap.tap.recent[snap.tap.recent.length - 1];
  assert.ok(newest, "the tap saw no rows, so 'capture never stopped' is not established");
  const nowZ = new Date(newest.tS * 1000).toISOString().slice(11, 19);
  t.diagnostic(`readout: ${snap.trace}`);
  t.diagnostic(`the viewport is tracing ${m[1]}Z; the socket's newest row is ${nowZ}Z`);
  assert.ok(m[1] < nowZ, `the viewport is not scrubbed: tracing ${m[1]}Z with the edge at ${nowZ}Z`);

  // ---- what the readout claims ----
  const glow = /afterglow (\d+) × ([\d.]+ (?:ms|s|min)) back to ([\d:]+)Z/.exec(snap.trace);
  // What the SOCKET delivered in the second before the slice's: independent of the pyramid and of
  // the page, so a red below says which of "no rows existed" and "rows existed and the pane said
  // there were none" it was.
  const [hh, mm, ss] = m[1].split(":").map(Number);
  const sliceS = hh * 3600 + mm * 60 + ss;
  const tapS = snap.tap.recent.map((r) => {
    const d = new Date(r.tS * 1000);
    return d.getUTCHours() * 3600 + d.getUTCMinutes() * 60 + d.getUTCSeconds() + (r.tS % 1);
  });
  const rowsBefore = tapS.filter((at) => at >= sliceS - 1 && at < sliceS).length;
  // The tap is a bounded ring that also restarts on a stream header, so "0 rows" is only evidence
  // when its oldest row is older than the second in question — stated rather than assumed.
  const socketSays = tapS.length && tapS[0] <= sliceS - 1
    ? `the socket delivered ${rowsBefore} row(s) in the second before ${m[1]}Z`
    : `the tap holds no rows from before ${m[1]}Z (its oldest is ${tapS.length ? (tapS[0] - sliceS).toFixed(2) : "—"} s from it), so the socket cannot say`;
  t.diagnostic(socketSays);
  assert.ok(glow, `no afterglow is stated on a scrubbed viewport whose earlier rows the page says it holds ` +
    `(${snap.counts}); ${socketSays}: ${JSON.stringify(snap.trace)}`);
  t.diagnostic(`afterglow: ${glow[1]} rows of ${glow[2]}, back to ${glow[3]}Z`);
  assert.ok(Number(glow[1]) >= 1, "the readout claims no glowing rows");
  assert.equal(glow[2], m[2].replace(" cell", ""),
    "an afterglow row must be the same kind of cell as the slice — it is the row before it, at the " +
    "level the pane was drawn at, not a window of its own");
  // **The claim.** The glow is back where the VIEWPORT is, not where capture is. A buffer of
  // recently-arrived frames would be glowing at the live edge, which is where this is not.
  assert.ok(glow[3] <= m[1] && glow[3] < nowZ,
    `the afterglow reaches back to ${glow[3]}Z behind a slice at ${m[1]}Z, with the live edge at ` +
    `${nowZ}Z. On a scrubbed viewport the glow must be in the viewport's past, not at the edge.`);

  // ---- and what the pixels show ----
  //
  // The afterglow and the bloom are the only ACHROMATIC ink in the strip (`TraceStyle.shade`), so a
  // grey pixel there is a shadow and a chromatic one is the current slice. That separation is what
  // lets this count them without a second copy of the ramp.
  const s = strip(obs.img, obs.rect);
  t.diagnostic(`strip: ${s.slicePx} ramp px (current slice), ${s.greyPx} achromatic px ` +
    `(afterglow + bloom), ${s.holdPx} max-hold px`);
  assert.ok(s.slicePx > 20, "the current slice is not drawn, so there is nothing for a glow to be behind");
  assert.ok(s.greyPx > 40,
    `only ${s.greyPx} achromatic pixels in the strip — the readout claims ${glow[1]} glowing rows and ` +
    "nothing is drawn behind the line");

  // ---- and where the rest of the claim is asserted, and WHY it is not asserted here ----
  //
  // "The shadows are four EARLIER ROWS, not four copies of this one" is the other half, and two
  // attempts to read it off the framebuffer both measured the wrong thing. First a count of columns
  // whose shadow sits clear of the line: 27, 25 and 7 on three identical runs, because consecutive
  // rows of a steady FM band genuinely are similar and the count was measuring the weather. Then the
  // shape of the offset distribution, which survived one tree and died in the next: with T-484
  // reverted a row is a 1 s max-hold rather than a 40 ms one, so rows are smoother still, and the
  // spread collapsed from 21–30 px to 7.
  //
  // The second failure is the informative one, because it is not an instrument problem. **A shadow
  // that coincides with the current line is hidden by construction**: it is a SHADOW_PX stroke drawn
  // UNDER a wider, fully opaque core. So the visible separation between the glow and the line is a
  // fact about how much the band moved in that window — real, and nothing to do with whether the
  // afterglow is derived from the pane's own past. Any pixel threshold over it is a fixture liveliness
  // meter wearing a feature's name, which is the exact class of proof this milestone keeps producing.
  //
  // So it is asserted where it is deterministic: `ui/test/surface-trace.test.ts` drives
  // `persistenceSlices` over a fixture of four rows at four known dB and asserts the VALUES — at the
  // edge the glow is the three rows before the newest, scrubbed one cell back it is the two before
  // THAT and never contains the newest, and the two answers differ. What this tier adds, and the
  // unit tier cannot, is that the whole chain runs in a browser on a viewport genuinely behind the
  // live edge: the readout above, and the ink below.
  //
  // The separation is still measured, and reported, because a future tree where it collapses to zero
  // is worth seeing in the log even though it is not a failure.
  const x0 = Math.round(obs.rect.x), y0 = Math.round(obs.rect.y);
  const offsets = [];
  for (let x = 0; x < s.w; x++) {
    if (s.cols[x] < 0) continue;
    let topGrey = -1;
    for (let y = 0; y < TRACE_PX && topGrey < 0; y++) {
      const d = ((y0 + y) * obs.img.width + (x0 + x)) * 4;
      const r = obs.img.data[d], g = obs.img.data[d + 1], b = obs.img.data[d + 2];
      if (isGreyInk(r, g, b)) topGrey = y;
    }
    if (topGrey >= 0) offsets.push(s.cols[x] - topGrey);
  }
  assert.ok(offsets.length > 50, `only ${offsets.length} columns carry any grey to measure`);
  const hist = new Map();
  for (const o of offsets) hist.set(o, (hist.get(o) ?? 0) + 1);
  const mode = [...hist.entries()].sort((a, b) => b[1] - a[1])[0][0];
  const away = offsets.filter((o) => Math.abs(o - mode) >= 2).length;
  t.diagnostic(`DIAGNOSTIC (not asserted — see above): topmost-grey offset above the line: mode ` +
    `${mode} px (the bloom's own edge is (GLOW_PX − SLICE_PX)/2 = 2), ${hist.size} distinct values, ` +
    `max ${Math.max(...offsets)}, ${((away / offsets.length) * 100).toFixed(1)}% of columns ≥ 2 px ` +
    "off the mode — this is how lively the band was across these rows, not whether the glow is the " +
    "rows before this one");
  await page.eval("window.__hkTap.resume()");
  assert.deepEqual(page.exceptions, [], "uncaught exception while measuring the afterglow");
});
