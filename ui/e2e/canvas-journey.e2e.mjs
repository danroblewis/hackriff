// **The canvas journey, end to end, through the MOCK SDR** (T-481).
//
// Three user-visible defects reached the user in one afternoon and every suite in this repo stayed
// green: a retune that tears the live capture down (T-497), a live tile that goes grey in the middle
// after being panned off-screen and back (T-495), and purple tiles with a re-render loop on stream
// loss (T-499). They are all one journey — open, look around, retune, look again — and nothing drove
// that journey. This file is that journey, in one flow, on one page, over one mock SDR.
//
// ——— WHAT EACH ASSERTION IS A PROPERTY OF ———
//
// The failure this repo keeps catching is a sound proof of an *adjacent* question (T-480 counted a
// retry storm rather than the addressing it named; T-487's argmax read its own tie-break; T-491's
// premise was measurably false). So each test says, in its header, what its evidence is a property
// **of** — and none of them is "the page looked right":
//
//   1. PAN/ZOOM     — the `/api/tiles` requests **on the wire** (status), and the pane's
//                     unobserved-grey pixel share against **`/api/coverage`'s own** unobserved-cell
//                     share for the same band. Not "tiles appeared".
//   2. RETUNE       — **rows delivered on the websocket after the commit instant, carrying the NEW
//                     centre in the header in force when they arrived**, plus pixels in the pane's
//                     freshest strip. A test that checks only the socket passes on a silent stream;
//                     one that checks only that rows exist passes on rows from before the retune.
//   3. OFF AND BACK — the pane's grey share **for the identical stated viewport, before and after**
//                     the round trip, with the server asked whether it gained coverage in between.
//                     The trap is that the stale tile IS drawn: the claim is COVERAGE, not presence.
//                     **It reaches T-460's half of that and not T-495's** — a 13 s round trip cannot
//                     finish a 256 s tile — which its own header measures and says.
//   4. STREAM LOSS  — the composited framebuffer's magenta pixel count (given a measured premise
//                     that no `unknown` coverage — the one legitimately purple mark on this surface
//                     — is in the window), and **counted** texture uploads and failed requests
//                     settling across successive windows. Not "it looked still".
//
// ——— WHY THE MOCK SDR, AND WHICH SCENE ———
//
// CLAUDE.md's rule for this tier: e2e drives the system THROUGH the device interface, never files
// straight into the pipeline. The shared `--replay` backend is not a device — it reports no
// frequency grid, so every retune control on it is correctly stated-and-disabled and a press posts
// nothing. `surface-retune.e2e.mjs` tests 1–4 would have been a green suite over a shipped retune
// defect for exactly that reason. So this file brings up its own `hk serve --device mock:…`.
//
// The scene is the FM fixture (100.8 MHz ± 1.2 MHz of real broadcast band) behind the mock, and the
// **second band is made by the journey itself**: the retune in test 2 moves the front end, so the
// coverage map ends the run holding two separated observed regions with genuinely-never-sampled
// spectrum on either side and between. That is cheaper than stitching fixtures, it commits no new
// binary (fixtures/ is LFS and `.sigmf-data` must stay a 134-byte pointer), and it is more honest:
// the grey between the bands is grey because this run's radio never looked there.
//
// ——— THE TRAPS THIS TIER HAS ALREADY EARNED ———
//
// T-478: never assert on a raw readout string — a following pane's chrome ends in its offset from
// the live edge and that offset drifts with wall-clock lag, so `equal` goes red under load and
// `notEqual` goes green on drift alone. Every readout here is parsed for a NUMBER and compared with
// a tolerance, or compared field-by-field against the same field.
// T-473: the CDP port is ephemeral now, and `startBackend` steps past an occupied port — but this
// file still takes its own port rather than the shared default.
// T-466: `selftest.mjs` builds only the `/surface.html` bundle, so the app-page faults this file
// would need have no per-guard attribution there. Non-vacuity is recorded in each test's header,
// from the defect the ticket names, run against `main`.
import test, { after } from "node:test";
import assert from "node:assert/strict";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

const ART = process.env.HK_E2E_ARTIFACTS ?? path.join(UI_DIR, "e2e", "artifacts");

/** The minimap strip along the BOTTOM of the canvas (`MINIMAP_PX` in app/centre/surface.ts). */
const MINIMAP_PX = 110;
/** The spectrum-trace strip carved off the TOP of each pane (`TRACE_PX`, T-457). */
const TRACE_PX = 96;

/**
 * **THE grey**, as the compositor writes it: `CELL_MARKS[UNOBSERVED]` is `[0.155, 0.16, 0.18]` and
 * the framebuffer holds it as bytes. ±1 per channel is the driver's rounding and nothing else — no
 * two adjacent ramp colours are that close. Same constant, same derivation, as `surface-colour`.
 */
const GREY_RGB = [Math.round(0.155 * 255), Math.round(0.16 * 255), Math.round(0.18 * 255)];

/** The pane's retune control, and the device routes a press may reach. */
const PANE_ACTION = '.hk-surface-viewport[data-viewport="pane"] .hk-surface-action';
const ZOOM = { shift: true }; // a FREQUENCY zoom; T-472 stops a plain wheel at either axis's bound.

// ---------------------------------------------------------------------------
// The instruments, installed before any of the page's own scripts. OBSERVE ONLY.
// ---------------------------------------------------------------------------

/**
 * The websocket tap (T-487's shape, minus the hold — nothing here needs one instant).
 *
 * What it records that `app-trace`'s does not: **the wall-clock instant of every row and every
 * header, and the geometry that was in force when the row arrived.** T-497's question is "are rows
 * still arriving, at the NEW centre, AFTER the retune", and that is a join of three facts that only
 * exist together on the wire. `app-trace`'s tap deliberately *clears* its recent rows on a header,
 * which is right for its claim and would erase this one.
 *
 * It forwards everything and changes nothing: a script that altered what the page does would make
 * this tier a test of a page nobody ships.
 */
const WS_TAP = `(() => {
  const Base = WebSocket;
  const tap = { sockets: [], opens: 0, closes: 0, errors: 0, headers: [], rows: [], geom: null };
  window.__hkWs = tap;
  const observe = (e) => {
    if (typeof e.data === "string") {
      try {
        const h = JSON.parse(e.data);
        if (h && h.kind === "spectrum" && h.bandwidth_hz > 0 && h.fft_size >= 1) {
          tap.geom = { centerHz: h.center_hz, bandwidthHz: h.bandwidth_hz };
          tap.headers.push({ atMs: Date.now(), centerHz: h.center_hz, bandwidthHz: h.bandwidth_hz });
        }
      } catch (_) {}
      return;
    }
    const buf = e.data;
    if (!(buf instanceof ArrayBuffer) || buf.byteLength < 36 || !tap.geom) return;
    const dv = new DataView(buf);
    if (dv.getUint8(0) !== 1) return;   // REC_DATA
    if (dv.getUint8(1) & 1) return;     // FLAG_GATED: no samples in this record
    tap.rows.push({ atMs: Date.now(), centerHz: tap.geom.centerHz, bandwidthHz: tap.geom.bandwidthHz });
    if (tap.rows.length > 6000) tap.rows.splice(0, 3000);
  };
  class TapSocket extends Base {
    constructor(...a) {
      super(...a);
      tap.sockets.push(this);
      this.addEventListener("open", () => { tap.opens++; });
      this.addEventListener("close", () => { tap.closes++; });
      this.addEventListener("error", () => { tap.errors++; });
      this.addEventListener("message", (e) => { try { observe(e); } catch (_) {} });
    }
  }
  window.WebSocket = TapSocket;
})();`;

/**
 * The GL tap: **counted texture uploads**, which is what a re-render loop actually spends.
 *
 * Draw calls are the wrong counter here and it matters: the surface renders from a `requestAnimation
 * Frame` loop that runs whether or not anything changed (`ui/src/surface/preview.ts`), so draw calls
 * are ~60/s forever and could never "settle". A tile arriving is a `texImage2D`/`texSubImage2D`
 * (`TileCache.upload`, which is the stat `preview-main.ts` shows as "N uploads" — a readout the app
 * page does not carry). T-499's left-to-right sweep is the renderer re-uploading, so uploads are the
 * number that must go quiet when the stream does.
 */
const GL_TAP = `(() => {
  const g = { uploads: 0, draws: 0 };
  window.__hkGl = g;
  const wrap = (P, name, key) => {
    const f = P && P.prototype && P.prototype[name];
    if (typeof f !== "function") return;
    P.prototype[name] = function (...a) { g[key]++; return f.apply(this, a); };
  };
  for (const P of [window.WebGL2RenderingContext, window.WebGLRenderingContext]) {
    wrap(P, "texImage2D", "uploads");
    wrap(P, "texSubImage2D", "uploads");
    wrap(P, "drawArrays", "draws");
    wrap(P, "drawElements", "draws");
  }
})();`;

const INIT = `${WS_TAP}\n${GL_TAP}`;

// ---------------------------------------------------------------------------
// The one backend, the one browser, the one page — the journey
// ---------------------------------------------------------------------------

let opening = null;
const journey = () => (opening ??= open());
after(async () => {
  const j = await opening?.catch(() => null);
  j?.browser?.close();
  j?.backend?.stop();
});

async function open() {
  // Its own port, not the tier's default: this backend is KILLED by test 4, and the shared one is
  // every other file's.
  const backend = await startBackend({ port: 8801, mockDevice: true });
  let browser = null;
  try {
    const covered = await waitForCoverage(backend, 90000);
    browser = await Browser.open();
    const page = await browser.page(undefined, { initScript: INIT });
    assert.equal(await page.goto(`${backend.origin}/#token=${backend.token}`), "load");
    await page.waitFor("the app shell to mount its surface slot",
      `!!document.querySelector('.sf-canvas')`, { timeoutMs: 20000 });
    await page.waitFor("the app's surface to finish addressing",
      `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
    const note = (await page.$text(".sf-note")) ?? "";
    assert.ok(!/could not be addressed|WebGL2 is unavailable/.test(note), `the surface refused to mount: ${note}`);
    await page.waitFor("a pane readout to exist",
      `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 20000 });
    await page.waitFor("the per-pane retune control to be on the page",
      `!!document.querySelector('${PANE_ACTION}')`, { timeoutMs: 20000 });
    // The instruments must have actually installed, or every claim below is about nothing.
    assert.equal(await page.eval("typeof window.__hkWs === 'object' && typeof window.__hkGl === 'object'"), true,
      "the observers did not install, so nothing below is measuring the page");
    await page.frames(4);
    return { page, browser, backend, covered };
  } catch (e) {
    browser?.close();
    backend.stop();
    throw e;
  }
}

// ---------------------------------------------------------------------------
// Reading the page, and reading the server
// ---------------------------------------------------------------------------

/** Every viewport row, as the user reads it. From the DOM, never from client bookkeeping (T-454). */
const ROWS = `JSON.stringify([...document.querySelectorAll('.hk-surface-viewport')].map((v) => {
  const b = v.querySelector('.hk-surface-action');
  return {
    id: v.querySelector('.hk-surface-id')?.textContent ?? '',
    viewport: v.getAttribute('data-viewport'),
    following: v.getAttribute('data-following') === 'true',
    where: v.querySelector('.hk-surface-where')?.textContent ?? '',
    level: v.querySelector('.hk-surface-level')?.textContent ?? '',
    counts: v.querySelector('.hk-surface-counts')?.textContent ?? '',
    disabled: b ? b.disabled : null,
    why: v.querySelector('.hk-surface-why')?.textContent ?? '',
  };
}))`;

const rows = async (page) => JSON.parse(await page.eval(ROWS));
const pane0 = async (page) => {
  const r = (await rows(page)).filter((x) => x.viewport === "pane")[0];
  assert.ok(r, "the page is drawing no pane at all");
  return r;
};

/**
 * The pane's frequency window, parsed from `.hk-surface-where`.
 *
 * Numbers, never the string: T-478. The readout states MHz to 3 dp, so it is known to about ±500 Hz
 * — every comparison against it carries that as its tolerance, and nothing here compares it for
 * equality as text.
 */
function windowOf(where) {
  const m = /^([\d.]+) MHz ± ([\d.]+) (Hz|kHz|MHz|GHz)/.exec(where);
  assert.ok(m, `the pane readout is not a frequency window: ${JSON.stringify(where)}`);
  const mult = { Hz: 1, kHz: 1e3, MHz: 1e6, GHz: 1e9 }[m[3]];
  const centerHz = Number(m[1]) * 1e6, halfHz = Number(m[2]) * mult;
  return { centerHz, halfHz, loHz: centerHz - halfHz, hiHz: centerHz + halfHz, spanHz: 2 * halfHz };
}

const MHz = (hz) => (hz / 1e6).toFixed(3);
const spanOf = (w) => `${MHz(w.loHz)}–${MHz(w.hiHz)} MHz`;

/**
 * `GET` against the backend, as the app's own client would — retrying its backpressure.
 *
 * `/api/tiles` answers `503` over `cost.in_flight_limit` concurrent reads (T-454) and this file's
 * own browser holds reads in flight, so a bare `fetch` here manufactures the refusal and then reads
 * it as an answer. A `503` is "busy now", never "no".
 */
async function get(backend, path, { tries = 40, waitMs = 200 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${backend.origin}${path}`, { headers: { authorization: `Bearer ${backend.token}` } });
    if (r.ok) return r.json();
    if (r.status !== 503 || i >= tries) assert.fail(`GET ${path} -> ${r.status}`);
    await new Promise((res) => setTimeout(res, waitMs));
  }
}

/**
 * **The server's own answer to "was this ever sampled?"** over a band and (optionally) a window.
 *
 * Counted the way `/api/coverage` states it and the way T-476's helper counts it — `state ===
 * "observed"`, never "not unobserved", because `unknown` is its own state and folding it into
 * coverage is precisely the grey-honesty error this surface exists to refuse.
 */
async function coverage(backend, { loHz, hiHz }, t0S = null, t1S = null, cells = 256, rows_ = 8) {
  const q = new URLSearchParams({ f_lo: String(Math.round(loHz)), f_hi: String(Math.round(hiHz)),
    cells: String(cells), rows: String(rows_) });
  if (t0S !== null && t1S !== null) { q.set("t0", String(Math.round(t0S))); q.set("t1", String(Math.round(t1S))); }
  const cov = await get(backend, `/api/coverage?${q}`);
  const cs = cov?.any?.cells ?? [];
  const observed = cs.filter((c) => c?.state === "observed").length;
  const unknown = cs.filter((c) => c?.state === "unknown").length;
  const unobserved = cs.length - observed - unknown;
  // **The shares are OF THE CELLS WHERE THE ANSWER IS KNOWN**, i.e. `unknown` is excluded from the
  // denominator rather than counted on either side.
  //
  // `unknown` means "the record that would say whether we looked is gone" (T-423) — a third state
  // that is neither grey nor a level, and on a young backend it is simply everything before
  // `horizon.oldest_record_s`. Counting it as coverage would overstate what the radio saw; counting
  // it as unobserved would demand grey where grey would be a lie. Measured on this fixture: over the
  // last 20 s the tuned window is 2305 observed, 0 unobserved and 1383 unknown purely because the
  // server is younger than the window asked for. Of the cells where it is known, it is 100 %
  // observed — which is the fact the pane is being compared against.
  const known = observed + unobserved;
  return { total: cs.length, observed, unknown, unobserved, known,
    observedShare: known ? observed / known : 0,
    unobservedShare: known ? unobserved / known : 0 };
}

/** Poll until the mock has put something in the coverage map, so the page opens ON the capture. */
async function waitForCoverage(backend, timeoutMs) {
  const t0 = Date.now();
  for (;;) {
    const c = await coverage(backend, { loHz: 1e6, hiHz: 6e9 }, null, null, 128, 32).catch(() => null);
    if (c && c.observed > 0) return { observed: c.observed, ms: Date.now() - t0 };
    if (Date.now() - t0 > timeoutMs) throw new Error(`the mock SDR put nothing in the coverage map in ${timeoutMs} ms`);
    await new Promise((r) => setTimeout(r, 500));
  }
}

/**
 * **The frequency cell size the pane says it DREW at**, from the level in its own readout and the
 * lattice the route declares — never re-derived here.
 *
 * Why every coverage comparison in this file needs it. `/api/coverage` answers at whatever `cells`
 * it is given; the pane answers at the level the frame resolved. Ask the server for 256 cells across
 * a band the pane drew in 8 and the two are not comparing the same question: a coarse cell is
 * observed if *anything* in it was observed (docs/16 §8.5a — "a coarser cell is a maximum over more
 * cells"), so the pane can legitimately draw no grey at all over a band the server calls 89 %
 * unobserved at a finer grid. Measured exactly that way on two consecutive runs of this file: 0.0 %
 * and then 44.1 % of the pane grey against the same 88.7 % / 88.8 % from the server, the difference
 * being nothing but which level happened to be resident. A comparison whose answer depends on that
 * is a comparison of two different claims.
 */
function levelOf(row) {
  const m = /\(level (\d+)\/(\d+)\)/.exec(row.level);
  assert.ok(m, `the pane states no level: ${JSON.stringify(row.level)}`);
  return { levelF: Number(m[1]), levelT: Number(m[2]) };
}

/** The lattice's finest frequency cell, from the route. Read once per backend. */
async function latticeCellHz(backend) {
  const j = await get(backend, "/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8");
  const hz = j?.axes?.frequency?.cell_hz;
  assert.ok(hz > 0, `the route declares no frequency cell size: ${JSON.stringify(j?.axes)}`);
  return hz;
}

/** The capture window the mock front end reports it is using, right now. */
async function tunedWindow(backend) {
  const nav = await get(backend, "/api/navigation");
  const w = nav.windows?.[0];
  assert.ok(w, `the mock reported no capture window: ${JSON.stringify(nav.windows)}`);
  return { loHz: w.f_lo_hz, hiHz: w.f_hi_hz, spanHz: w.span_hz, centerHz: w.center_hz };
}

// ---------------------------------------------------------------------------
// Reading the pixels
// ---------------------------------------------------------------------------

/** The rectangle a pane draws its MEASUREMENT into: the canvas, minus the map strip and the trace. */
function paneRectOf(rect, dpr) {
  const paneH = rect.h * dpr - MINIMAP_PX;
  const traceH = Math.max(0, Math.min(TRACE_PX, Math.floor(paneH / 3)));
  return { x: rect.x, w: rect.w, y: rect.y + traceH / dpr, h: (paneH - traceH) / dpr };
}

/**
 * **The newest third of a pane is the LIVE-EDGE ZONE, and no grey claim in this file is made over
 * it.** Found by this file, and recorded here because it is the reason for the rect every grey
 * assertion uses.
 *
 * Over a viewport the server reports 100 % observed, fully resident (`0 coarse stand-ins · 0
 * pending`), the pane's THE-grey share pulses between 0 % and 54 % from one frame to the next — and
 * the profile by vertical tenth says exactly where it lives: `100% 53% 35% 0% 0% 0% 0% 0% 0% 0%` in
 * one run, `15% 0% 0% …` two minutes later, `33% 2% 0% …` in a third. **Every grey pixel is in the
 * newest tenths and none is below them**, and it drains as each live-tile revalidation lands. That
 * is a real defect — grey is the one colour that may only mean "the radio never looked", and these
 * rows were recorded and are served — but its amplitude varies by a factor of fifty run to run, so
 * a threshold over it would be a coin toss rather than a guard. It is reported as a diagnostic in
 * every test below, and asserted nowhere.
 *
 * Below the zone the same measurement reads 0.0 % in every run, which is where the claims live.
 */
const LIVE_EDGE_ZONE = 0.40;

/** The pane's data rect minus the newest few per cent, which are legitimately not yet folded. */
function bodyRect(pane, from = 0.06, to = 1.0) {
  return { x: Math.round(pane.x), w: Math.round(pane.w),
    y: Math.round(pane.y + pane.h * from), h: Math.round(pane.h * (to - from)) };
}

async function paneGeometry(page) {
  const rect = await page.$rect(".sf-canvas");
  assert.ok(rect && rect.w > 300 && rect.h > 260, `the canvas has no usable box: ${JSON.stringify(rect)}`);
  const dpr = await page.eval("window.devicePixelRatio || 1");
  return { rect, dpr, pane: paneRectOf(rect, dpr) };
}

/** Per-pixel classification of a rect: THE grey, magenta, and the plain census beside them. */
function inspect(img, rect) {
  const { x, y, w, h } = rect;
  let grey = 0, magenta = 0, n = 0;
  let worstMagenta = null;
  for (let py = y; py < y + h; py++) {
    for (let px = x; px < x + w; px++) {
      const d = (py * img.width + px) * 4;
      const r = img.data[d], g = img.data[d + 1], b = img.data[d + 2];
      n++;
      if (Math.abs(r - GREY_RGB[0]) <= 1 && Math.abs(g - GREY_RGB[1]) <= 1 && Math.abs(b - GREY_RGB[2]) <= 1) grey++;
      // MAGENTA: red and blue both well above green. The colour ramp is black → deep blue → cyan →
      // yellow → red → white (ui/src/cmap.ts) and has no magenta anywhere on it; nor do PENDING,
      // BACKDROP, THE grey or the `no-level` mark. The ONE mark on this surface that lands here is
      // the `unknown` hatch (T-413/T-423, ink rgb 112,77,133) — which is why test 4 measures, from
      // the server, that its window holds no `unknown` coverage before it concludes anything.
      if (r > g + 20 && b > g + 20 && (r + b) / 2 > 60) {
        magenta++;
        if (!worstMagenta) worstMagenta = { x: px, y: py, rgb: [r, g, b] };
      }
    }
  }
  // **Where the grey is, by vertical tenth** (newest rows first). A share alone cannot tell a band
  // of stale rows from a uniformly-grey pane, and those are different defects with different owners:
  // a band that moves with the live edge is the tile's coverage plane lagging its rows; a band in the
  // middle of otherwise-present rows is T-495; grey everywhere is a pane over unsampled spectrum.
  const bands = [];
  for (let k = 0; k < 10; k++) {
    const y0 = y + Math.floor((h * k) / 10), y1 = y + Math.floor((h * (k + 1)) / 10);
    let gb = 0, nb = 0;
    for (let py = y0; py < y1; py++) {
      for (let px = x; px < x + w; px++) {
        const d = (py * img.width + px) * 4;
        nb++;
        if (Math.abs(img.data[d] - GREY_RGB[0]) <= 1 && Math.abs(img.data[d + 1] - GREY_RGB[1]) <= 1 &&
            Math.abs(img.data[d + 2] - GREY_RGB[2]) <= 1) gb++;
      }
    }
    bands.push(nb ? gb / nb : 0);
  }
  return { n, grey, magenta, worstMagenta, greyShare: n ? grey / n : 0, bands,
    bandsText: bands.map((v) => `${(v * 100).toFixed(0)}%`).join(" "), census: census(img, rect) };
}

/** How many pixels of `rect` differ between two frames. `surface-colour`'s `diff`, unchanged. */
function diff(a, b, rect) {
  let n = 0;
  for (let py = rect.y; py < rect.y + rect.h; py++) {
    for (let px = rect.x; px < rect.x + rect.w; px++) {
      const i = (py * a.width + px) * 4, j = (py * b.width + px) * 4;
      if (a.data[i] !== b.data[j] || a.data[i + 1] !== b.data[j + 1] || a.data[i + 2] !== b.data[j + 2]) n++;
    }
  }
  const total = rect.w * rect.h;
  return { pixels: n, total, share: total ? n / total : 0 };
}

/**
 * Let the page draw, then take one frame.
 *
 * Deliberately NOT `surface-colour`'s settle-until-two-frames-agree: on a FOLLOWING pane the
 * picture legitimately never stops moving — rows arrive ~25 times a second (T-484) — so a settle
 * would always spend its whole timeout and then report "unsettled" for a page behaving correctly.
 * Every claim in this file is a large-scale share of the pane rather than a per-pixel comparison,
 * so one frame after the tiles have had time to arrive is the right instrument, and the wait is
 * named rather than tuned.
 */
async function draw(page, ms = 2500) {
  await page.frames(4);
  await new Promise((r) => setTimeout(r, ms));
  await page.frames(4);
  return page.shot();
}

/** A real render rather than a flat fill — the same shape of claim as `app-surface`/`live-edge`. */
const isRender = (c) => c.distinct >= 32 && c.dominantShare < 0.9;

/**
 * **A grey share averaged over several frames, seconds apart** — the instrument every grey claim in
 * this file uses, and `live-edge.e2e.mjs`'s discipline applied to a different quantity.
 *
 * Why not one screenshot. A following pane's grey share *pulses*: measured here across six single
 * frames of the same fully-observed viewport it read 0.9 %, 3.6 %, 7.4 %, 9.4 %, 12.3 % and 18.7 %,
 * and the band profile says why — the grey is entirely in the newest tenths and drains as each
 * live-tile revalidation lands. A threshold against one frame of that is a coin toss, which is the
 * one thing a standing regression guard must not be. The mean over five frames across ten seconds
 * is a claim about the pane's behaviour rather than about the instant the shot was taken, and it
 * moves the same way the defect does.
 */
async function sampleGrey(page, rect, { n = 5, gapMs = 2000 } = {}) {
  const shares = [], all = [];
  let last = null;
  for (let i = 0; i < n; i++) {
    if (i) await new Promise((r) => setTimeout(r, gapMs));
    await page.frames(3);
    last = inspect(await page.shot(), rect);
    shares.push(last.greyShare);
    all.push(last);
  }
  return {
    mean: shares.reduce((a, b) => a + b, 0) / shares.length,
    max: Math.max(...shares), min: Math.min(...shares),
    text: shares.map((v) => `${(v * 100).toFixed(1)}%`).join(" "),
    last, all,
  };
}

/**
 * **Wait until the pane is drawing its OWN tiles**, from the page's own residency readout.
 *
 * Why every pixel measurement in this file is gated on it. A tile that has not arrived is drawn as
 * a coarser ancestor, upscaled and hatched (`FALLBACK_MARK`), or as the pending mark — deliberately
 * never as grey (docs/16 §5.5, T-441's F3). So a frame taken while tiles are still in flight reads
 * *less* grey than the truth, and by exactly how much depends on which level happened to be resident
 * when the shot was taken. Measured that way on two consecutive runs of this file: 89.4 % and then
 * 33.7 % of the same pane grey against the same 88.8 % from the server. The number was not noisy;
 * the question was.
 *
 * `.hk-surface-counts` is the frame's own report — `${tiles} tiles · ${fallbacks} coarse stand-ins ·
 * ${pending} pending` — which is `PaneReport`, i.e. what the renderer actually drew with, not a
 * second calculation that could disagree with the pixels. It is the same instrument `surface-colour`
 * pins residency with, for the same reason.
 *
 * It **reports** rather than throws when the pane will not converge: a surface that never becomes
 * resident is a finding the assertion after it should describe.
 */
async function waitForResident(page, { timeoutMs = 25000, everyMs = 400 } = {}) {
  const t0 = Date.now();
  let last = "";
  for (;;) {
    const row = await pane0(page);
    last = row.counts;
    const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(row.counts);
    if (m && Number(m[1]) > 0 && Number(m[2]) === 0 && Number(m[3]) === 0) {
      return { resident: true, counts: last, ms: Date.now() - t0 };
    }
    if (Date.now() - t0 > timeoutMs) return { resident: false, counts: last, ms: Date.now() - t0 };
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/**
 * Put the pane back on the live edge if a gesture froze it.
 *
 * The app's one live control is a button whose TEXT is its state ("Live" / "Paused",
 * `ui/src/app/centre/surface.ts`), so this reads the state it is in rather than toggling blind — a
 * blind toggle would pause a following pane exactly half the time.
 */
async function goLive(page) {
  const paused = await page.eval(
    `!!([...document.querySelectorAll('button')].find((b) => b.textContent === 'Paused'))`);
  if (!paused) return false;
  await page.click(`[...document.querySelectorAll('button')].find((b) => b.textContent === 'Paused')`);
  await page.frames(4);
  return true;
}

/**
 * **How many Hz one pixel of horizontal drag moves the view, MEASURED on this page.**
 *
 * Not assumed, and not re-derived from `view.ts`'s arithmetic: a harness that computed the mapping
 * itself would be a second implementation of the very thing the product does, and would agree with
 * its own copy when the product drifted. A short drag, the page's own stated window before and
 * after, and the ratio — which also reports whether the drag moved the view at all, so a navigation
 * that silently does nothing fails as a navigation rather than as the claim after it.
 */
async function calibratePan(page, at) {
  const v0 = windowOf((await pane0(page)).where);
  const dx = at.rect.w * 0.12;
  await page.drag(at, { x: at.x + dx, y: at.y }, 6);
  await page.frames(3);
  const v1 = windowOf((await pane0(page)).where);
  const hzPerPx = (v1.centerHz - v0.centerHz) / dx;
  // Put it back, so calibration is not also a navigation.
  await page.drag({ x: at.x + dx, y: at.y }, { x: at.x, y: at.y }, 6);
  await page.frames(3);
  return { hzPerPx, moved: Math.abs(v1.centerHz - v0.centerHz) };
}

/**
 * Pan and zoom (frequency only) until the pane's stated window is inside `want`.
 *
 * Pan AND zoom, because zooming alone cannot get there: the surface opens on bounds padded well
 * outside the observed region (measured on this fixture: the pane opens at 98.6–120.8 MHz for a
 * capture at 99.6–102.0 MHz), so a wheel about the canvas centre converges on 109.7 MHz — outside
 * the tuned window — however many notches it is given. That is exactly how the first run of this
 * file failed, and it is a harness fact rather than a product one.
 *
 * SHIFT-held throughout (see [[ZOOM]]): every claim these navigations set up is a frequency claim,
 * and since T-472 a plain wheel stops as soon as EITHER axis reaches a bound — on a young record the
 * time axis is pinned before the first notch, so a plain wheel would move neither.
 */
async function navigate(page, at, want, { steps = 40 } = {}) {
  const wantC = (want.loHz + want.hiHz) / 2, wantSpan = want.hiHz - want.loHz;
  const cal = await calibratePan(page, at);
  const trail = [];
  let view = windowOf((await pane0(page)).where);
  for (let i = 0; i < steps; i++) {
    view = windowOf((await pane0(page)).where);
    trail.push(spanOf(view));
    if (view.loHz >= want.loHz && view.hiHz <= want.hiHz) break;
    const off = wantC - view.centerHz;
    // Pan when far off, THEN zoom, then trim the centre. Ordering a pan ahead of the zoom whenever
    // the offset is small makes the two fight: a wheel about the canvas centre keeps the centre, so
    // shrinking the span grows the offset *relative to the span* and re-triggers the pan, and the
    // loop oscillates a few kHz outside the target for its whole budget — which is how the third run
    // of this file spent 40 gestures landing 116 kHz short.
    const pan = async () => {
      const dx = Math.max(-at.rect.w * 0.42, Math.min(at.rect.w * 0.42, off / cal.hzPerPx));
      await page.drag(at, { x: at.x + dx, y: at.y }, 8);
    };
    if (Math.abs(cal.hzPerPx) > 0 && Math.abs(off) > view.spanHz * 0.35) await pan();
    else if (view.spanHz > wantSpan) await page.wheel(at, -400, ZOOM);
    else if (Math.abs(cal.hzPerPx) > 0 && Math.abs(off) > view.spanHz * 0.04) await pan();
    else break; // narrower than the target and centred on it, yet not inside: report, do not loop.
    await page.frames(3);
  }
  return { view, cal, steps: trail.length, trail };
}

/** Re-open the app on its own opening window — the deterministic way back from a wild gesture. */
async function reopen(page, backend) {
  // A DIFFERENT url each time. `location.href = same-document-#hash` is a same-document navigation:
  // no reload, no `load` event, and `goto` then reports `timeout` for a page that is working
  // perfectly. (That is how the second run of this file failed.) A unique query makes it a real one.
  assert.equal(await page.goto(`${backend.origin}/?e2e=${Date.now()}#token=${backend.token}`), "load");
  await page.waitFor("the app shell to mount its surface slot",
    `!!document.querySelector('.sf-canvas')`, { timeoutMs: 20000 });
  await page.waitFor("the app's surface to finish addressing",
    `(document.querySelector('.sf-note')?.textContent ?? "").length > 0`, { timeoutMs: 60000 });
  await page.waitFor("a pane readout to exist",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 20000 });
  await page.waitFor("the per-pane retune control to be on the page",
    `!!document.querySelector('${PANE_ACTION}')`, { timeoutMs: 20000 });
  await page.frames(4);
}

async function centreOf(page) {
  const r = await page.$rect(".sf-canvas");
  return { x: r.x + r.w * 0.5, y: r.y + r.h * 0.35, rect: r };
}

/**
 * **The recent window the coverage question has to be asked over, and why it is not the default.**
 *
 * `/api/coverage` with no `t0`/`t1` answers over the *capture window*, which on this server is a
 * fixed 120 s while the record itself begins at `horizon.oldest_record_s`. Rows wholly before that
 * are `unknown` — "we no longer know whether we looked" — and on a young backend that is 50 of 64
 * rows. Measured on this very fixture: 99.6–102 MHz over the default window is 3200 `unknown` and
 * 896 `observed`; the same band over the last 10 s is 1984 `observed` and 64 `unobserved`.
 *
 * `unknown` is neither grey nor a level (T-413/T-423: forgetting is not a measurement of nothing),
 * so folding it into either side of a grey comparison would make the comparison meaningless. Asking
 * over a recent window is how the question "is there data here?" gets an answer about the span the
 * pane is actually showing.
 *
 * **The rows must not be finer than the store's own cells either**: the same 20 s asked as 64 rows
 * (0.3 s each) comes back with 1536 `unknown` cells and as 8 rows (2.5 s each) with none, because a
 * sub-second row falls inside a level-0 cell rather than covering one. Hence `coverage`'s default of
 * 8 rows, and RECENT_S long enough that 8 of them are comfortably coarser than a 1 s cell.
 */
const RECENT_S = 20;
const recent = () => { const now = Date.now() / 1000; return [now - RECENT_S, now]; };

// ===========================================================================
// 1. PAN AND ZOOM: tiles where data exists, grey where it does not, zero 4xx
// ===========================================================================
//
// **A property of the wire and of the server's coverage answer**, in two halves that fail for
// different reasons and are therefore asserted separately:
//
//   (a) over an aggressive gesture, NO `/api/tiles` request is refused 4xx. This is T-480's guard at
//       the app page (`surface-address.e2e.mjs` holds it on `/surface.html`, and T-482 landed the
//       per-axis ceiling `(9,1)` that makes it achievable). A 503 is T-454's backpressure — "busy
//       now", never "no" — and is counted separately, never as a refusal.
//   (b) grey is drawn where, and only where, the RADIO NEVER LOOKED. Not judged by eye and not
//       judged against a constant: the pane's grey share is compared with `/api/coverage`'s own
//       unobserved share for the band the pane says it is showing, in two states — the whole
//       1 MHz–6 GHz surface (mostly never sampled) and the tuned window (entirely sampled). The
//       CONTRAST between the two is the assertion, because an absolute threshold would be a claim
//       about how much of the surface this fixture happens to cover.
test("1. an aggressive pan/zoom makes no invalid tile request, and greys only what was never sampled",
  async (t) => {
    const { page, backend, covered } = await journey();
    t.diagnostic(`mock SDR coverage after ${covered.ms} ms: ${covered.observed} observed cells`);
    const at = await centreOf(page);
    const before = await pane0(page);
    const firstIdx = page.requests.length;

    // The gesture: zoom out hard on both axes and on each axis alone, then pan to each corner at
    // that zoom — an INDEX bound is reached by panning, a LEVEL bound by zooming, and the two halves
    // of the clamp fail independently (T-480).
    for (let i = 0; i < 20; i++) {
      await page.wheel(at, 120);
      if (i % 3 === 0) await page.wheel(at, 120, { shift: true });
      if (i % 3 === 1) await page.wheel(at, 120, { alt: true });
      if (i % 5 === 0) await page.frames(2);
    }
    await page.frames(6);
    for (const [dx, dy] of [[1, 0], [-2, 0], [0, 1], [0, -2]]) {
      await page.drag(at, { x: at.x + at.rect.w * dx * 0.4, y: at.y + at.rect.h * dy * 0.4 }, 6);
      await page.frames(3);
    }
    await page.frames(10);

    const after = await pane0(page);
    // NON-VACUITY: the gestures moved the view. Compared as a NUMBER (T-478) — the stated span, in
    // Hz — never as the readout string, which carries a drifting live-edge offset.
    assert.notEqual(windowOf(after.where).spanHz, windowOf(before.where).spanHz,
      `the gestures did not change the viewport's span at all (${before.where}), so nothing below is a measurement`);

    const tiles = page.requests.slice(firstIdx).filter((r) => r.url.includes("/api/tiles"));
    assert.ok(tiles.length >= 4, `the gesture made ${tiles.length} tile requests; too few to conclude anything`);
    const refused = tiles.filter((r) => r.status !== null && r.status >= 400 && r.status !== 503);
    const busy = tiles.filter((r) => r.status === 503).length;
    const addr = (u) => {
      const q = new URL(u).searchParams;
      return `${q.get("level_f")}/${q.get("level_t")} @ ${q.get("f_index")},${q.get("t_index")} x${q.get("cells") ?? 256}`;
    };
    t.diagnostic(`${tiles.length} tile requests over the gesture: ${refused.length} refused, ${busy} backpressure (503)`);
    assert.deepEqual(refused.slice(0, 5).map((r) => `${r.status} ${addr(r.url)}`), [],
      `${refused.length}/${tiles.length} tile requests over an aggressive pan/zoom were REFUSED. ` +
      "T-480/T-482: navigation must resolve only to realizable addresses, and the route declares " +
      "how far up it can be read.");

    // ——— (b) grey only where the radio never looked ———
    //
    // Re-open first. The gesture above deliberately left the view anywhere at all — that was its
    // job — and `preview.ts` decides the opening window once, at load, from the observed coverage,
    // so a reload is the deterministic way back to a known live view. Navigating back by hand would
    // be a second implementation of the opening-window arithmetic, which is the drift shape this
    // repo keeps refusing.
    await reopen(page, backend);
    const w0 = await tunedWindow(backend);
    const at2 = await centreOf(page);

    // State A: inside the tuned window, where the radio has been looking for the whole run.
    const inner = { loHz: w0.loHz + w0.spanHz * 0.2, hiHz: w0.hiHz - w0.spanHz * 0.2 };
    const zi = await navigate(page, at2, inner);
    t.diagnostic(`into the tuned window in ${zi.steps} gesture(s) ` +
      `(${(zi.cal.hzPerPx / 1e3).toFixed(1)} kHz per drag px, calibration moved ${(zi.cal.moved / 1e3).toFixed(0)} kHz): ` +
      `${spanOf(zi.view)} ⊂ ${spanOf(w0)}`);
    assert.ok(zi.cal.moved > 0, "a horizontal drag moved the view by 0 Hz, so this page cannot be navigated at all");
    assert.ok(zi.view.loHz >= w0.loHz && zi.view.hiHz <= w0.hiHz,
      `the viewport never got inside the tuned window: ${spanOf(zi.view)} vs ${spanOf(w0)} after ${zi.steps} steps`);
    let g = await paneGeometry(page);
    const cell0Hz = await latticeCellHz(backend);
    /** Ask the server at the resolution the PANE drew at — see [[levelOf]]. */
    const atPaneLevel = async (view) => {
      const row = await pane0(page);
      const cellHz = cell0Hz * 2 ** levelOf(row).levelF;
      const n = Math.max(1, Math.min(4096, Math.round(view.spanHz / cellHz)));
      return { cov: await coverage(backend, view, ...recent(), n), cellHz, n };
    };
    const insideRes = await waitForResident(page);
    t.diagnostic(`INSIDE residency after ${insideRes.ms} ms: ${insideRes.counts}`);
    const insideG = await sampleGrey(page, bodyRect(g.pane, LIVE_EDGE_ZONE));
    const insideEdge = await sampleGrey(page, bodyRect(g.pane, 0.06, LIVE_EDGE_ZONE), { n: 3, gapMs: 700 });
    const insidePix = insideG.last;
    const { cov: insideCov, cellHz: insideCellHz, n: insideN } = await atPaneLevel(zi.view);
    t.diagnostic(`pane level: ${insideN} cells of ${(insideCellHz / 1e3).toFixed(1)} kHz across the viewport`);
    t.diagnostic(`INSIDE ${spanOf(zi.view)}: pane ${(insideG.mean * 100).toFixed(1)} % THE grey (mean of ` +
      `${insideG.text}); ` +
      `server (last ${RECENT_S} s) ${(insideCov.unobservedShare * 100).toFixed(1)} % unobserved OF KNOWN ` +
      `(${insideCov.observed} obs / ${insideCov.unobserved} unobs / ${insideCov.unknown} unk of ${insideCov.total}); ` +
      `census ${insidePix.census.distinct} distinct, dominant ${(insidePix.census.dominantShare * 100).toFixed(0)} %`);
    t.diagnostic(`INSIDE grey by vertical tenth over the WHOLE pane (newest first): ` +
      `${insideG.all[insideG.all.length - 1].bandsText}`);
    // DIAGNOSTIC, not a claim — see [[LIVE_EDGE_ZONE]]. A live edge drawing recorded rows as
    // never-looked-at is a real defect; its amplitude varies fiftyfold, so it is reported, not gated.
    t.diagnostic(`INSIDE the live-edge zone (newest ${(LIVE_EDGE_ZONE * 100).toFixed(0)} % of the pane): ` +
      `${(insideEdge.mean * 100).toFixed(1)} % THE grey (${insideEdge.text}) over spectrum the server ` +
      "reports fully observed");

    // State B: zoomed OUT until the tuned window is a minority of the pane, so most of what is on
    // screen is spectrum this run's radio never looked at. Frequency only, so the time span — and
    // with it which rows are inside the record — does not change between the two states.
    let wide = zi.view;
    for (let i = 0; i < 20 && wide.spanHz < w0.spanHz * 6; i++) {
      await page.wheel(at2, 400, ZOOM);
      await page.frames(2);
      wide = windowOf((await pane0(page)).where);
    }
    g = await paneGeometry(page);
    const wideRes = await waitForResident(page);
    t.diagnostic(`OUTSIDE residency after ${wideRes.ms} ms: ${wideRes.counts}`);
    const wideG = await sampleGrey(page, bodyRect(g.pane, LIVE_EDGE_ZONE), { n: 3 });
    const widePix = wideG.last;
    const { cov: wideCov, cellHz: wideCellHz, n: wideN } = await atPaneLevel(wide);
    t.diagnostic(`pane level: ${wideN} cells of ${(wideCellHz / 1e3).toFixed(1)} kHz across the viewport`);
    t.diagnostic(`OUTSIDE ${spanOf(wide)} (${(wide.spanHz / w0.spanHz).toFixed(1)}x the tuned window): ` +
      `pane ${(wideG.mean * 100).toFixed(1)} % THE grey (mean of ${wideG.text}); server (last ${RECENT_S} s) ` +
      `${(wideCov.unobservedShare * 100).toFixed(1)} % unobserved OF KNOWN ` +
      `(${wideCov.observed} obs / ${wideCov.unobserved} unobs / ${wideCov.unknown} unk of ${wideCov.total})`);

    // The premise, from the SERVER, so neither half can be vacuous: the two states really do differ
    // in whether the radio ever looked. Without this both assertions below would be claims about the
    // fixture rather than about the renderer.
    assert.ok(insideCov.known > 200 && insideCov.observedShare > 0.9,
      `the tuned window is ${(insideCov.observedShare * 100).toFixed(1)} % observed of ${insideCov.known} known ` +
      `cells by the server's own account over the last ${RECENT_S} s — there is no 'data exists' region here ` +
      "to require tiles for");
    assert.ok(wideCov.known > 200 && wideCov.unobservedShare > 0.4,
      `the zoomed-out view is ${(wideCov.unobservedShare * 100).toFixed(1)} % unobserved of ${wideCov.known} known ` +
      "cells by the server's own account — there is no 'never sampled' region on screen to require grey for");

    // THE CLAIM, both directions, each against the server's own number rather than a constant.
    assert.ok(insideG.mean < 0.05,
      `${(insideG.mean * 100).toFixed(1)} % of a pane over a band the server reports ` +
      `${(insideCov.observedShare * 100).toFixed(1)} % OBSERVED is drawn as THE grey (five frames: ${insideG.text}; ` +
      `by vertical tenth, newest first: ${insidePix.bandsText}). The surface is claiming the radio never looked ` +
      "at data the server holds — CLAUDE.md: 'we have it but didn't render it' is a bug, and grey is the one " +
      "colour that may only mean 'never sampled'. The band profile says where: grey concentrated in the newest " +
      "tenths is the LIVE EDGE drawing recorded-but-not-yet-covered rows as never-looked-at.");
    // Residency is a PREMISE of the pixel comparison, not a nicety: an unarrived tile draws a hatched
    // coarse stand-in, never grey, so a pane still filling reads less grey than the truth.
    assert.ok(wideRes.resident, `the pane never became fully resident (${wideRes.counts} after ${wideRes.ms} ms), ` +
      "so its grey share is a measurement of what has arrived rather than of what was sampled");
    assert.ok(Math.abs(wideG.mean - wideCov.unobservedShare) < 0.20,
      `grey does not track coverage: the pane draws ${(wideG.mean * 100).toFixed(1)} % THE grey over a band ` +
      `the server reports ${(wideCov.unobservedShare * 100).toFixed(1)} % unobserved. Grey is the load-bearing ` +
      "claim that the radio never looked, and it must be neither more nor less than the truth.");
    assert.ok(isRender(insidePix.census),
      `the pane over observed spectrum is not a render: ${insidePix.census.distinct} distinct, ` +
      `dominant ${(insidePix.census.dominantShare * 100).toFixed(0)} %`);
    assert.deepEqual(page.exceptions, [], "uncaught exception during the pan/zoom leg");
  });

// ===========================================================================
// 2. RETUNE: the stream survives it, and rows keep arriving AT THE NEW CENTRE
// ===========================================================================
//
// **T-497.** A property of **rows on the websocket, joined to the header in force when they arrived,
// joined to the wall-clock instant of the commit** — three facts that only exist together on the
// wire, which is why the tap records all three per row.
//
// The two adjacent questions this must not answer instead, both named in the ticket:
//   - "is the socket open?" — passes with a silent stream;
//   - "are there rows?" — passes on rows from BEFORE the retune.
// So the assertion is over rows whose arrival instant is after the commit AND whose geometry is the
// new centre. Neither half alone is the claim.
//
// The pixel half is beside it, because a row delivered is not a row drawn (T-460's lesson exactly):
// the pane's freshest strip must be a real render after the retune, not a flat fill.
test("2. a retune keeps the stream alive, and live rows keep arriving at the NEW centre", async (t) => {
  const { page, backend } = await journey();
  const w0 = await tunedWindow(backend);

  // The control plans for the viewport, so put the viewport somewhere else inside the recorded band
  // and let the page name the destination. This is T-476's path — the one T-497 suspects — driven
  // exactly as a user drives it: a discrete press on a persistent control.
  const at = await centreOf(page);
  // Following first: test 1 left the view zoomed out, and a pane frozen behind the growing edge is
  // correctly refused by the control ("a retune changes only what is captured from now on"). That
  // refusal is right, and it is not the subject here.
  if (await goLive(page)) t.diagnostic("the pane had been frozen by test 1's gestures; put back on the live edge");
  // Somewhere OFF the current centre but still inside the recorded band, so the retune the control
  // plans really moves the front end and really lands on spectrum the recording has signal in.
  const target = { loHz: w0.centerHz + w0.spanHz * 0.06, hiHz: w0.hiHz - w0.spanHz * 0.06 };
  const zi = await navigate(page, at, target);
  t.diagnostic(`positioned for the retune in ${zi.steps} gesture(s): ${spanOf(zi.view)}`);
  // What the retune needs, rather than an exact box: a viewport inside the tuned window (so the
  // control is takeable rather than "survey overview") and OFF its centre (so a retune to it really
  // moves the front end). Asserting the exact target would be asserting the navigation, not the
  // condition the test needs.
  // READOUT_TOL: the readout states MHz to 3 dp, so a window is known to ~±500 Hz on each edge.
  // That rounding — not slack in the claim — is why this is not a bare `>=`.
  const READOUT_TOL_HZ = 3e3;
  assert.ok(zi.view.loHz >= w0.loHz - READOUT_TOL_HZ && zi.view.hiHz <= w0.hiHz + READOUT_TOL_HZ,
    `the viewport is not inside the tuned window before the retune: ${spanOf(zi.view)} vs ${spanOf(w0)}`);
  assert.ok(Math.abs(zi.view.centerHz - w0.centerHz) > w0.spanHz * 0.05,
    `the viewport sits on the tuned centre (${MHz(zi.view.centerHz)} vs ${MHz(w0.centerHz)} MHz), so a ` +
    "retune to it would not move the front end and there would be no re-plumb to survive");
  let row = await pane0(page);
  t.diagnostic(`before the retune: window in force ${spanOf(w0)}, viewport ${spanOf(windowOf(row.where))}`);

  // A press needs an enabled control naming a destination. If the drag left the viewport wider than
  // one live window the control correctly says "survey overview" — zoom in until it is takeable.
  for (let i = 0; i < 12 && row.disabled !== false; i++) {
    await page.wheel(at, -400, ZOOM);
    await page.frames(3);
    row = await pane0(page);
  }
  assert.equal(row.disabled, false,
    `the retune control is not takeable on this live viewport: ${JSON.stringify(row.why)}`);
  const said = /^Retune to ([\d.]+) MHz at ([\d.]+) MHz span/.exec(row.why);
  assert.ok(said, `the enabled control names no destination: ${JSON.stringify(row.why)}`);
  const wantCenterHz = Number(said[1]) * 1e6, wantSpanHz = Number(said[2]) * 1e6;
  assert.ok(Math.abs(wantCenterHz - w0.centerHz) > 1e4,
    `the retune would not move the front end (${MHz(wantCenterHz)} vs ${MHz(w0.centerHz)} MHz), so ` +
    "there is no re-plumb here to survive");

  // The commit instant, read from THE PAGE's clock so it is the same clock the tap stamps rows with.
  const tCommit = await page.eval("Date.now()");
  const beforeRows = await page.eval("window.__hkWs.rows.length");

  // `applyDeviceAction` posts the covering rate and then the centre; a rate change re-plumbs, so the
  // centre can land inside the settle gap and come back `device_busy` — one capture at a time, which
  // is the rule, not a fault. Press again, and assert on where the FRONT END ends up.
  let w1 = w0;
  for (let attempt = 0; attempt < 5; attempt++) {
    await page.click(`document.querySelector('${PANE_ACTION}')`);
    await page.frames(4);
    for (let i = 0; i < 20; i++) {
      w1 = await tunedWindow(backend);
      if (Math.abs(w1.centerHz - wantCenterHz) < 100) break;
      await new Promise((r) => setTimeout(r, 250));
    }
    if (Math.abs(w1.centerHz - wantCenterHz) < 100) break;
  }
  assert.ok(Math.abs(w1.centerHz - wantCenterHz) < 100,
    `the front end is at ${MHz(w1.centerHz)} MHz and the control said ${MHz(wantCenterHz)} MHz ` +
    `(toast: ${await page.$text("#toast")})`);
  assert.ok(w1.spanHz <= wantSpanHz + 1,
    `the capture is ${MHz(w1.spanHz)} MHz wide; the control named ${MHz(wantSpanHz)} MHz`);
  t.diagnostic(`retuned: ${MHz(w0.centerHz)} MHz / ${MHz(w0.spanHz)} MHz span -> ` +
    `${MHz(w1.centerHz)} MHz / ${MHz(w1.spanHz)} MHz span`);

  // ——— the measurement window: ten seconds of capture AFTER the front end got there ———
  const tSettled = await page.eval("Date.now()");
  await new Promise((r) => setTimeout(r, 10000));
  const tap = JSON.parse(await page.eval(`JSON.stringify({
    opens: window.__hkWs.opens, closes: window.__hkWs.closes, errors: window.__hkWs.errors,
    sockets: window.__hkWs.sockets.length,
    openNow: window.__hkWs.sockets.filter((s) => s.readyState === 1).length,
    headers: window.__hkWs.headers.slice(-8),
    total: window.__hkWs.rows.length,
    after: window.__hkWs.rows.filter((r) => r.atMs > ${tSettled}).map((r) => r.centerHz),
  })`));
  const atNew = tap.after.filter((c) => Math.abs(c - w1.centerHz) < 1000).length;
  const atOld = tap.after.filter((c) => Math.abs(c - w0.centerHz) < 1000).length;
  t.diagnostic(`socket: ${tap.sockets} opened, ${tap.opens} open events, ${tap.closes} closes, ` +
    `${tap.errors} errors, ${tap.openNow} open now; rows ${beforeRows} -> ${tap.total}; ` +
    `in the 10 s after the retune settled: ${tap.after.length} rows, ${atNew} at the NEW centre, ${atOld} at the old`);
  t.diagnostic(`headers seen (last 8): ${tap.headers.map((h) => `${MHz(h.centerHz)}MHz/${MHz(h.bandwidthHz)}MHz`).join(" ")}`);

  // THE CLAIM, and all three halves of it are needed.
  assert.ok(tap.openNow >= 1,
    `no websocket is open ${((Date.now() - tSettled) / 1000).toFixed(0)} s after the retune ` +
    `(${tap.sockets} sockets ever, ${tap.closes} closes, ${tap.errors} errors). T-417: a retune must ` +
    "not disconnect stream consumers.");
  assert.ok(atNew >= 20,
    `only ${atNew} spectrum rows arrived at the NEW centre (${MHz(w1.centerHz)} MHz) in the 10 s after ` +
    `the retune settled — ${tap.after.length} rows arrived in total, ${atOld} of them still carrying the ` +
    `OLD centre. This is T-497: the live capture went and did not come back. ` +
    `(sockets ${tap.sockets}, opens ${tap.opens}, closes ${tap.closes}, errors ${tap.errors})`);

  // …and a row delivered is not a row drawn (T-460). The pane's freshest strip, after the retune.
  const g = await paneGeometry(page);
  const fresh = bodyRect(g.pane, 0.06, 0.30);
  const img = await page.shot(path.join(ART, "canvas-journey-retune.png"));
  const c = census(img, fresh);
  t.diagnostic(`newest rows after the retune: ${c.distinct} distinct, dominant ${(c.dominantShare * 100).toFixed(0)} %`);
  assert.ok(isRender(c),
    `the newest rows of the following pane are a FLAT fill after the retune (${c.distinct} distinct, ` +
    `dominant ${(c.dominantShare * 100).toFixed(0)} %): the rows arrived and the view did not fill.`);
  assert.deepEqual(page.exceptions, [], "uncaught exception during the retune");
});

// ===========================================================================
// 3. A LIVE TILE PANNED OFF-SCREEN AND BACK HAS NO GREY GAP
// ===========================================================================
//
// **T-495**, and the trap is written into the ticket: *a test that only checks the tile is drawn
// passes today, because the stale tile IS drawn.* The claim is about COVERAGE.
//
// So this is a property of **the pane's THE-grey share for the identical stated viewport, before and
// after the round trip**, with the server asked separately whether it gained coverage while the tile
// was away. The "before" measurement is the control: same page, same window, same backend, so an
// absolute threshold — which would be a claim about the fixture — is not needed. A resident tile
// whose middle went stale while off-screen returns with rows the server holds drawn as "never
// looked", and that is exactly an INCREASE in this share.
//
// ——— WHAT THIS TEST CANNOT REACH, MEASURED (T-495) ———
//
// **It passed before T-495 was fixed, and that is not a bug in it — it is the half of the defect a
// browser run of this length cannot get to.** T-495's mechanism is that a live tile's *eligibility*
// for revalidation ends when the live edge crosses the tile's own end, so a tile that spent the last
// of its own life off screen keeps whatever the server had when it was last looked at, forever. The
// edge has to LEAVE the tile for that to happen.
//
// It cannot here. A pane draws at `RENDER_CELLS` = 256 cells of the lattice's 1 s base cell, so one
// level-0 tile is **256 s of capture**; the round trip below is ~13 s (printed by the diagnostic, so
// the claim is checkable from the output rather than taken from this comment). The edge therefore
// never leaves the tile, T-460's live-edge refresh covers the whole trip, and the measured grey share
// is 0.00 % before and 0.00 % after. What this test IS a property of is that: **T-460's refresh
// survives a pan away and back.** Worth having, and not the T-495 case.
//
// The case where eligibility expires is guarded in `ui/test/surface-cache.test.ts` ("a live tile
// panned off screen and back fills in"), which drives the same six gestures through the cache over
// an 8 s tile so the boundary is crossed, in both arms — the edge still inside on return (which
// always worked) and the edge gone past it (which never re-asked). Making this tier reach it would
// mean a 256 s browser run, which is four times the whole file's budget.
test("3. a live tile panned off-screen and back shows no grey gap: its coverage is complete on return",
  async (t) => {
    const { page, backend } = await journey();
    const g = await paneGeometry(page);
    // Below the live-edge zone ([[LIVE_EDGE_ZONE]]), which is why the return below is given time to
    // scroll: the rows that arrived while the tile was off screen ARE the newest rows on return, so
    // measuring immediately would put the subject inside the one band whose grey cannot be read.
    const body = bodyRect(g.pane, LIVE_EDGE_ZONE);
    const at = await centreOf(page);

    // The pane must be FOLLOWING, or there is no live tile here to go stale.
    await goLive(page);
    const row = await pane0(page);
    assert.equal(row.following, true, "the pane is not following the live edge, so this test has no subject");

    // **The user's step 3: pan so the NEW tile is in view.** The subject of T-495 is a tile that
    // became live *because of the retune* — not one that has been on screen since before it. Test 2
    // has just moved the front end, so one canvas width along the new window is a place this page has
    // not drawn at this level since, and its tile is freshly live. Left rather than right only
    // because the retune landed on the upper half of the new window; the direction is not the claim.
    await page.drag(at, { x: at.x + at.rect.w * 0.8, y: at.y }, 10);
    await page.frames(4);
    const fresh = await waitForResident(page);
    t.diagnostic(`onto the freshly-live tile: ${spanOf(windowOf((await pane0(page)).where))}, ` +
      `resident after ${fresh.ms} ms (${fresh.counts})`);
    await new Promise((r) => setTimeout(r, 6000)); // let it accumulate rows while it is being watched

    // ——— on screen: the control measurement ———
    const beforeRes = await waitForResident(page);
    t.diagnostic(`residency before the round trip after ${beforeRes.ms} ms: ${beforeRes.counts}`);
    const beforeG = await sampleGrey(page, body, { n: 4, gapMs: 1500 });
    const beforePix = beforeG.last;
    const beforeWin = windowOf((await pane0(page)).where);
    const tAway = Date.now() / 1000;
    t.diagnostic(`on screen ${spanOf(beforeWin)}: ${(beforeG.mean * 100).toFixed(2)} % THE grey ` +
      `(mean of ${beforeG.text}), ${beforePix.census.distinct} distinct; ` +
      `by vertical tenth (newest first) ${beforePix.bandsText}`);
    // Non-vacuity, first half: there is a tile here, drawn, over data. A pane that was already grey
    // would make "no MORE grey" trivially true.
    assert.ok(beforeG.mean < 0.05,
      `the pane is already ${(beforeG.mean * 100).toFixed(1)} % grey before anything is panned, so ` +
      "this test has no clean baseline to compare a return against. The baseline is load-bearing: a high " +
      "one would make 'no MORE grey on return' true for free, which is the vacuity the ticket warns about.");
    assert.ok(isRender(beforePix.census), "the pane is not drawing a render before the pan, so there is no tile to lose");

    // ——— pan LEFT so the tile goes off-screen, and leave it away while rows keep arriving ———
    // A whole canvas width and a half: the viewport under test is entirely off the screen, which is
    // the user's step 4. Horizontal only — a vertical drag would unfollow the pane and the subject
    // would stop being a LIVE tile.
    await page.drag(at, { x: at.x + at.rect.w * 0.75, y: at.y }, 10);
    await page.frames(4);
    await page.drag(at, { x: at.x + at.rect.w * 0.75, y: at.y }, 10);
    await page.frames(4);
    const awayWin = windowOf((await pane0(page)).where);
    assert.ok(awayWin.loHz >= beforeWin.hiHz || awayWin.hiHz <= beforeWin.loHz,
      `the pan did not take the viewport off the tile under test: ${spanOf(awayWin)} still overlaps ${spanOf(beforeWin)}`);
    const AWAY_MS = 12000;
    await new Promise((r) => setTimeout(r, AWAY_MS));
    const tBack = Date.now() / 1000;

    // Non-vacuity, second half, FROM THE SERVER: it really did gain coverage of the band under test
    // while that band was off screen. Without this the claim would be "no grey appeared" in a span
    // where nothing was recorded either — an assertion about nothing.
    const gained = await coverage(backend, beforeWin, tAway, tBack, 64, 4);
    t.diagnostic(`while off screen (${(tBack - tAway).toFixed(0)} s), the server recorded ` +
      `${gained.observed}/${gained.total} cells OBSERVED over ${spanOf(beforeWin)}`);
    assert.ok(gained.observedShare > 0.5,
      `the server holds only ${(gained.observedShare * 100).toFixed(0)} % coverage of ${spanOf(beforeWin)} for the ` +
      `${(tBack - tAway).toFixed(0)} s the tile was off screen, so there is no 'span the server has data for' here`);

    // ——— pan back, to the SAME window, and settle ———
    await page.drag(at, { x: at.x - at.rect.w * 0.75, y: at.y }, 10);
    await page.frames(4);
    await page.drag(at, { x: at.x - at.rect.w * 0.75, y: at.y }, 10);
    await page.frames(6);
    // Let the off-screen rows scroll out of the live-edge zone and into the band this test can read.
    // Under the defect they are grey FOREVER ("never fills", T-495), so waiting cannot hide it; under
    // a correct client there was never a gap to wait out.
    const SCROLL_MS = 12000;
    await new Promise((r) => setTimeout(r, SCROLL_MS));
    const afterWin = windowOf((await pane0(page)).where);
    // The comparison is only meaningful over the same band: compared as NUMBERS with the readout's
    // own ±500 Hz rounding, never as strings (T-478).
    assert.ok(Math.abs(afterWin.centerHz - beforeWin.centerHz) < 5e3 &&
      Math.abs(afterWin.spanHz - beforeWin.spanHz) < 5e3,
      `the round trip did not return to the same viewport: ${spanOf(beforeWin)} -> ${spanOf(afterWin)}`);

    // Residency FIRST, and it is the sharp edge of this test: under T-495 the tile IS resident — that
    // is the whole defect — so waiting for residency cannot hide the bug, and it removes the one
    // innocent explanation for extra grey (a tile that simply has not arrived draws a hatched
    // stand-in, not grey, but a pane mid-fill is not the state the claim is about).
    const afterRes = await waitForResident(page);
    t.diagnostic(`residency after the round trip after ${afterRes.ms} ms: ${afterRes.counts}`);
    const afterG = await sampleGrey(page, body, { n: 4, gapMs: 1500 });
    const afterPix = afterG.last;
    // **The reach of this tier, printed rather than asserted** (see the header). A level-0 tile is
    // `RENDER_CELLS` x the lattice's base time cell; the round trip has to be longer than that for
    // the tile to finish while it is away, which is the T-495 case.
    // `RENDER_CELLS` (256, ui/src/surface/preview.ts) x this backend's base time cell — `axes.time.
    // cell_s`, which `/api/tiles` reports as 1.0 s here.
    const TILE_S = 256;
    t.diagnostic(`round trip ${(tBack - tAway).toFixed(0)} s against a level-0 tile of ${TILE_S} s: the live ` +
      "edge did NOT leave the tile, so this measures T-460's refresh surviving the trip, not T-495's " +
      "expired eligibility (guarded in ui/test/surface-cache.test.ts)");
    t.diagnostic(`back on screen ${spanOf(afterWin)}: ${(afterG.mean * 100).toFixed(2)} % THE grey ` +
      `(mean of ${afterG.text}), ${afterPix.census.distinct} distinct; ` +
      `by vertical tenth (newest first) ${afterPix.bandsText}`);

    // THE CLAIM: no more of the pane claims "never looked" than did before it was panned away.
    // The tolerance is one part in fifty of the pane — smaller than any horizontal band a user can
    // see, and far smaller than the gap the ticket describes (rows over a whole dwell).
    const TOL = 0.03;
    assert.ok(afterG.mean <= beforeG.mean + TOL,
      `the pane came back ${(afterG.mean * 100).toFixed(2)} % THE grey (four frames: ${afterG.text}) against ` +
      `${(beforeG.mean * 100).toFixed(2)} % before it was panned away (${beforeG.text}) — over a band the server reports `  +
      `${(gained.observedShare * 100).toFixed(0)} % observed for the whole ${(tBack - tAway).toFixed(0)} s it was ` +
      `off screen, and with the tile fully resident (${afterRes.counts}). Measured BELOW the live-edge zone, ` +
      `by vertical tenth (newest first) ${afterPix.bandsText}. This is T-495: a resident live tile is not fresh ` +
      "just because it is resident; its freshness is its coverage up to the live edge.");
    // …and it is still a picture, not a blank that happens to hold no grey.
    assert.ok(isRender(afterPix.census),
      `the pane is not a render after the round trip: ${afterPix.census.distinct} distinct, ` +
      `dominant ${(afterPix.census.dominantShare * 100).toFixed(0)} %`);
    assert.deepEqual(page.exceptions, [], "uncaught exception over the off-screen round trip");
  });

// ===========================================================================
// 4. STREAM LOSS: grey, never purple, and no re-render thrash
// ===========================================================================
//
// **T-499**, and its DoD says how: *demonstrated by killing the backend under the browser tier
// rather than by simulating it*, and *no re-render thrash, measured as draw calls settling rather
// than "it looked still"*. So this test kills `hk serve` with the live page in front of it. It runs
// LAST because the backend does not come back.
//
// Two properties, measured two ways:
//   (a) COLOUR — magenta pixels in the pane. The ramp (black → blue → cyan → yellow → red → white)
//       has no magenta, and neither has THE grey, PENDING, BACKDROP or the `no-level` mark. The one
//       legitimate magenta on this surface is the `unknown` hatch, so the premise "this window holds
//       no `unknown` coverage" is READ FROM THE SERVER before the kill, when there is still a server
//       to ask. Without that premise the count would be ambiguous, which is the adjacent-question
//       mistake in miniature.
//   (b) THRASH — **counted** texture uploads (the tile cache's own `upload`, wrapped at the GL entry
//       point by the observer above) and failed `/api/tiles` requests, in two successive windows
//       after the kill. Draw calls are the WRONG counter here and the difference matters: the
//       surface renders from an unconditional rAF loop, so draw calls run at frame rate whether or
//       not anything changed and could never settle. A re-render loop spends uploads and requests.
/**
 * **T-499, and it is green now. It was `skip`ped and red, and both halves are worth keeping.**
 *
 * THE LOOP. This assertion failed on `main`, reproducibly: after SIGKILLing the backend, **56 failed
 * requests in the first 5 s and 55 in the second** on this rig (the ticket carries 182/181 from the
 * one it was filed on) — flat, no decay, which is a retry loop rather than a client winding down.
 * The cause was one outcome with nothing to pace it: T-479 made everything the server *said*
 * terminal, leaving *the server said nothing* as the only retryable case, and `acquire` runs for
 * every place on every frame. `tilecache.ts`'s silence backoff (`OFFLINE_BACKOFF_MS`) is the fix and
 * `ui/test/surface-cache.test.ts` holds the property; this is the demonstration, on a real socket.
 *
 * THE COLOUR. The magenta half passed here before the fix too — **0 magenta before and after** — and
 * that is not the guard being weak, it is the ticket's premise being wrong, which was worth finding.
 * A killed server never answers, so nothing new reaches the ramp at all. What DOES produce the
 * user's purple was measured separately, by restarting `hk serve` under a live page with a fresh
 * data dir: **54.3 % of the pane at rgb(112,77,133) at +3 s, 20.4 % at +8 s, 9.3 % at +15 s, 0 at
 * +25 s**. That ink is `CELL_MARKS[CELL.UNKNOWN]`, decoded from a well-formed 200 — `hk-api`'s
 * `unknown_rows` serves EVERY row as `"unknown"` while no record survives anywhere, so a server that
 * has just lost its history paints the whole window in the fourth state until it re-accumulates.
 * Nothing malformed, no NaN, and the ramp never sees it: `cellMark` returns the hatch before `x` is
 * read. The count below still stands as a guard — if a value ever does reach the ramp and land in
 * that corner of the cube, this is where it shows.
 *
 * Draw calls are the WRONG counter here and the file says so where it counts them: the surface
 * renders from an unconditional rAF loop (~2.5 M draws per run) and could never "settle". The
 * subject is **failed requests on the wire**, which is a property of the client's retry policy and
 * of nothing else.
 */
test("4. a killed backend degrades to grey with no purple and no re-render thrash", async (t) => {
  const { page, backend } = await journey();
  await goLive(page);
  const g = await paneGeometry(page);
  // **The freshest third of the pane, not the whole of it.** The magenta test needs a strip whose
  // rows are inside the record, because a row wholly before `horizon.oldest_record_s` is `unknown`
  // coverage and `unknown` is drawn as a MAGENTA HATCH (T-413/T-423) — a legitimate mark this
  // measurement must not have to tell apart from the defect. The newest rows are at the top, so the
  // strip is the top third, and the premise below is asked of the server over the matching window.
  const body = bodyRect(g.pane, 0.06, 0.60);

  // The premise, from the server, while there still is one.
  const win = windowOf((await pane0(page)).where);
  const cov = await coverage(backend, win, ...recent(), 256);
  t.diagnostic(`before the kill, ${spanOf(win)} over the last ${RECENT_S} s: ${cov.observed} observed / ` +
    `${cov.unknown} unknown / ${cov.unobserved} unobserved of ${cov.total} cells`);
  assert.equal(cov.unknown, 0,
    `the pane's band holds ${cov.unknown} 'unknown'-coverage cells even over the last ${RECENT_S} s, and those ` +
    "are drawn as a MAGENTA hatch (T-413/T-423) — the magenta count below could not then distinguish " +
    "that legitimate mark from the defect, so this test would be measuring the wrong thing rather than failing");

  // **The magenta detector is not inert**, checked against the one mark on this surface that is
  // legitimately magenta: `CELL_MARKS[UNKNOWN]`'s ink, rgb(112, 77, 133) (T-413/T-423). If the
  // predicate did not classify that as magenta it would not classify an out-of-ramp colour either,
  // and the assertion below would be a guard against nothing. This is the one place the check can
  // live: the predicate is in this file, so a unit test elsewhere would be testing a copy.
  {
    const ink = { width: 1, data: Uint8Array.from([112, 77, 133, 255]) };
    assert.equal(inspect(ink, { x: 0, y: 0, w: 1, h: 1 }).magenta, 1,
      "the magenta predicate does not even fire on the surface's own `unknown` ink, so it cannot fire " +
      "on a colour the ramp cannot produce either");
    const cyan = { width: 1, data: Uint8Array.from([0, 179, 230, 255]) };
    assert.equal(inspect(cyan, { x: 0, y: 0, w: 1, h: 1 }).magenta, 0,
      "the magenta predicate fires on a colour that IS on the ramp (cyan), so it would fire on a " +
      "correct render");
  }

  const rowsBeforeKill = await page.eval("window.__hkWs.rows.length");
  const tBeforeKill = await page.eval("Date.now()");
  const baseImg = await draw(page);
  const basePix = inspect(baseImg, body);
  t.diagnostic(`before the kill: ${basePix.magenta} magenta px, ${(basePix.greyShare * 100).toFixed(2)} % THE grey, ` +
    `${basePix.census.distinct} distinct`);
  assert.equal(basePix.magenta, 0,
    `${basePix.magenta} magenta pixels were already on screen BEFORE the stream was killed ` +
    `(e.g. ${JSON.stringify(basePix.worstMagenta)}), so this test cannot attribute any to the loss`);

  // ——— the kill. SIGKILL, so nothing shuts anything down politely. ———
  const tKill = Date.now();
  const failedBefore = page.requests.filter((r) => r.error !== null).length;
  const uploadsAtKill = await page.eval("window.__hkGl.uploads");
  backend.stop();
  t.diagnostic("hk serve killed (SIGKILL) under the live page");

  // Two successive windows. The first is the legitimate reaction — in-flight requests fail, the
  // client notices, the page says so. The second is whether it SETTLED.
  const WINDOW_MS = 5000;
  await new Promise((r) => setTimeout(r, WINDOW_MS));
  const uploadsW1 = (await page.eval("window.__hkGl.uploads")) - uploadsAtKill;
  const failedW1 = page.requests.filter((r) => r.error !== null).length - failedBefore;
  const imgW1 = await page.shot();
  await new Promise((r) => setTimeout(r, WINDOW_MS));
  const uploadsW2 = (await page.eval("window.__hkGl.uploads")) - uploadsAtKill - uploadsW1;
  const failedW2 = page.requests.filter((r) => r.error !== null).length - failedBefore - failedW1;
  const imgW2 = await page.shot(path.join(ART, "canvas-journey-streamloss.png"));

  // **The dead-stream control for TEST 2's instrument**, and it costs nothing to take here. Test 2
  // asserts that ≥ 20 rows arrive at the new centre in ten seconds after a retune; that assertion is
  // only worth something if the same counter reads ZERO when the stream really is gone. It does —
  // measured on the same page, the same tap, the same counter, with the server killed. Without this
  // the passing half of test 2 would be a number nobody had ever seen fail.
  const wsAfterKill = JSON.parse(await page.eval(`JSON.stringify({
    total: window.__hkWs.rows.length,
    afterKill: window.__hkWs.rows.filter((r) => r.atMs > ${tKill}).length,
    beforeKill: window.__hkWs.rows.filter((r) => r.atMs > ${tBeforeKill} && r.atMs <= ${tKill}).length,
    openNow: window.__hkWs.sockets.filter((s) => s.readyState === 1).length,
  })`));
  t.diagnostic(`the tap across the kill: ${wsAfterKill.beforeKill} rows in the seconds BEFORE it ` +
    `(from ${rowsBeforeKill} total), ${wsAfterKill.afterKill} in the ${((Date.now() - tKill) / 1000).toFixed(0)} s ` +
    `after; ${wsAfterKill.openNow} socket(s) open now`);
  assert.equal(wsAfterKill.afterKill, 0,
    `${wsAfterKill.afterKill} spectrum rows arrived AFTER the server was killed, so the tap test 2 relies on ` +
    "does not actually go to zero when the stream dies — and test 2's ≥ 20 rows would then prove nothing");
  assert.ok(wsAfterKill.beforeKill > 0,
    "the tap recorded no rows in the seconds before the kill either, so it was not observing a live " +
    "stream and the zero above says nothing");

  const pixW2 = inspect(imgW2, body);
  const churn = diff(imgW1, imgW2, body);
  const draws = await page.eval("window.__hkGl.draws");
  t.diagnostic(`after the kill: uploads ${uploadsW1} in the first ${WINDOW_MS / 1000} s, ${uploadsW2} in the second; ` +
    `failed requests ${failedW1} then ${failedW2}; ${draws} draw calls total (the rAF loop, not the subject); ` +
    `${(churn.share * 100).toFixed(2)} % of the pane's pixels changed between the two windows`);
  t.diagnostic(`the pane, ${((Date.now() - tKill) / 1000).toFixed(0)} s after the kill: ${pixW2.magenta} magenta px, ` +
    `${(pixW2.greyShare * 100).toFixed(2)} % THE grey, ${pixW2.census.distinct} distinct, ` +
    `dominant ${(pixW2.census.dominantShare * 100).toFixed(0)} %`);

  // (a) THE COLOUR. Grey — or any mark the surface owns — is an answer; magenta is not a colour any
  //     measurement on this ramp can produce, so a user cannot tell it is not data.
  assert.equal(pixW2.magenta, 0,
    `${pixW2.magenta} pixels of the pane are MAGENTA after the stream died ` +
    `(e.g. ${JSON.stringify(pixW2.worstMagenta)}) over a band with no 'unknown' coverage. This is T-499: ` +
    "a value no measurement can produce is reaching the ramp. The answer is grey — the screen must " +
    "distinguish what was measured from what was not.");

  // (b) THE THRASH. Settling, counted: the second window's work is a small fraction of the first's.
  //     `+ 4` so a genuinely quiet pair (0 and 0, or 1 and 2) is not read as a ratio.
  assert.ok(uploadsW2 <= uploadsW1 * 0.5 + 4,
    `texture uploads did not settle after the stream died: ${uploadsW1} in the first ${WINDOW_MS / 1000} s ` +
    `and ${uploadsW2} in the second. This is T-499's loop — the renderer re-asking and re-drawing ` +
    "rather than settling. T-479's rule: retryable is the enumerated case, terminal is the default.");
  assert.ok(failedW2 <= failedW1 * 0.5 + 4,
    `the client is still hammering a dead server: ${failedW1} failed requests in the first ` +
    `${WINDOW_MS / 1000} s and ${failedW2} in the second. Re-asking cannot revive a dead stream.`);
  assert.ok(churn.share < 0.10,
    `${(churn.share * 100).toFixed(1)} % of the pane repainted between two windows ${WINDOW_MS / 1000} s apart ` +
    "with no server and no gesture — the left-to-right re-render sweep, still running");
  // Exceptions are reported rather than asserted here: a killed server legitimately produces network
  // errors, and which of those the app chooses to throw is T-499's business, not this guard's.
  if (page.exceptions.length) {
    t.diagnostic(`${page.exceptions.length} uncaught exception(s) after the kill: ` +
      page.exceptions.slice(0, 3).map((e) => e.text.slice(0, 160)).join(" | "));
  }
});
