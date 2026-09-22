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
//   5. REFUSED RETUNE — (T-508) the same press against a mock whose device REFUSES the retune once:
//                     the backend's run state (not finished, capture running) and rows at the new
//                     centre after the recovery. Its own backend, with a fault.
//   6. DEVICE GONE  — (T-508) a mock whose device goes away mid-retune: the backend reports the run
//                     ended, and the SURFACE states "Capture stopped" with the cause, having stated
//                     it was recovering first. Its own backend, with a fault.
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
import net from "node:net";
import path from "node:path";
import { Browser, census } from "./harness.mjs";
import { UI_DIR, startBackend } from "./backend.mjs";

/**
 * The base of the three ports this file's own backends take (the journey's, and one each for tests
 * 5 and 6). `startBackend` steps past an occupied port, so the default is safe against one other
 * run — but this file KILLS a backend in test 4 and starts two more after it, and two copies of
 * THIS file racing over one pool hand each other the port the other just freed. Overridable so an
 * intermittent in here can be measured the only way an intermittent can be: many copies at once.
 */
const PORT_BASE = Number(process.env.HK_E2E_JOURNEY_PORT ?? 8801);

/**
 * **Take over a port a killed `hk serve` just freed, and refuse everything on it.**
 *
 * T-470 stopped this tier from adopting somebody else's server at startup. Test 4 is the other half
 * of that hole: it KILLS its backend and then spends twenty seconds asserting that nothing reaches
 * the page — while the page reconnects to that same origin on a loop and the port sits free for
 * anyone to bind. Measured here with four copies of this file running at once: a concurrent run's
 * own `hk serve` took the freed port, the page reconnected to it, and **59, 156 and 215 rows**
 * arrived from a stranger's radio in three of twelve runs. Read literally that is "the killed
 * server kept streaming", which is false and unfalsifiable from inside the page.
 *
 * So the test holds the port instead of leaving it open. Every connection is destroyed on accept,
 * which is what a dead server looks like from the client's side, and no other process can bind it
 * while this test is making claims about it. `null` if it could not be taken within the grace
 * period — the caller fails rather than measuring something it cannot name.
 */
async function holdPort(port, { graceMs = 5000 } = {}) {
  const t0 = Date.now();
  do {
    const srv = net.createServer((s) => s.destroy());
    srv.on("error", () => {});
    const bound = await new Promise((res) => {
      srv.once("error", () => res(false));
      srv.listen(port, "127.0.0.1", () => res(true));
    });
    if (bound) return srv.unref();   // never a reason for this process to outlive its own tests
    await new Promise((r) => setTimeout(r, 25));
  } while (Date.now() - t0 < graceMs);
  return null;
}

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
 *
 * **`armDeath` / `deadAt` (test 4).** The instant the stream is observed to END, recorded in the
 * close handler itself so it is exact rather than the moment a poll happened to notice. Test 4
 * arms it immediately before it kills the backend; the first close after that is the boundary
 * everything in that test is counted against. It is armed rather than "the first close of the run"
 * because earlier tests reconnect, and it is one-shot because the client keeps retrying a dead
 * server and each refused attempt closes another socket — a boundary that marched forward with
 * them would shrink the measurement window to nothing.
 */
const WS_TAP = `(() => {
  const Base = WebSocket;
  const tap = { sockets: [], opens: 0, closes: 0, errors: 0, headers: [], rows: [], geom: null,
                closeAt: [], armDeath: false, deadAt: null };
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
      this.addEventListener("close", () => {
        tap.closes++;
        const at = Date.now();
        tap.closeAt.push(at);
        if (tap.armDeath && tap.deadAt === null) tap.deadAt = at;
      });
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

async function open({ port = PORT_BASE, mockFault = null } = {}) {
  // Its own port, not the tier's default: this backend is KILLED by test 4, and the shared one is
  // every other file's. Tests 5 and 6 bring up their own on other ports, each with a device FAULT
  // (T-508), so a fault can never reach the journey's backend.
  const backend = await startBackend({ port, mockDevice: true, mockFault });
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
  // that is neither grey nor a level. Since T-507 it is only what a server recorded and LOST (rows
  // between `horizon.recording_began_s` and `horizon.oldest_record_s`, or everything before the
  // latter when `horizon.forgotten` says so); time before a young server began recording is
  // `unobserved`, and grey. Counting `unknown` as coverage would overstate what the radio saw;
  // counting it as unobserved would demand grey where grey would be a lie — so it stays out of the
  // denominator either way.
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
  // T-505 put the TIER inside the same parenthesis ("… (detail tier, level 2/0)"), so the open
  // paren is no longer adjacent to the word `level`. Match the level alone.
  const m = /level (\d+)\/(\d+)\)/.exec(row.level);
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
 * **The newest 40 % of a pane: the LIVE-EDGE ZONE, and since T-532 it carries a claim of its own.**
 *
 * It did not, and that is why this nearly shipped. Over a viewport the server reported 100 %
 * observed, fully resident (`0 coarse stand-ins · 0 pending`), the pane's THE-grey share pulsed
 * between 0 % and 54 % from one frame to the next, and the profile by vertical tenth said exactly
 * where it lived: `100% 53% 35% 0% 0% 0% 0% 0% 0% 0%` in one run, `15% 0% 0% …` in another. Every
 * grey pixel was in the newest tenths and none below them, draining as each live-tile revalidation
 * landed. It was reported here as a diagnostic and asserted nowhere, on the reasoning that its
 * amplitude varied fiftyfold run to run and a threshold over it would be a coin toss.
 *
 * **That reasoning was wrong, and it cost a revert.** The amplitude varied because the quantity was
 * a *staleness*: the newest rows of a resident tile's coverage plane were written before those rows
 * were recorded, so how much grey a frame showed was how long ago the tile had last been re-asked.
 * A varying number is still a number that must be zero — grey is the one colour that may only mean
 * *the radio never looked*, and these rows were recorded and served. T-532 makes the renderer stop
 * drawing past the instant each answer's evidence reaches (`coverage.horizon.as_of_s`), so over a
 * band the server reports fully observed the honest answer here is **none at all**, in every frame,
 * whatever the refresh happens to be doing.
 *
 * So the claim is asserted, at [[EDGE_GREY_MAX]], and only where its premise is measured from the
 * server: a window that is >90 % observed. Over spectrum that was never sampled the newest rows are
 * legitimately grey and this zone says nothing.
 */
const LIVE_EDGE_ZONE = 0.40;
/**
 * The most grey the live-edge zone may show over a band the server reports fully observed.
 *
 * Five per cent, the same allowance the body of the pane already carries below — not a tuned number
 * but the same one, because it is the same claim about the same colour and there is no reason for
 * the newest rows to be held to a looser standard than the rest. Measured after T-532: **0.0 %**,
 * every frame of every run; before it, 10–38 %.
 */
const EDGE_GREY_MAX = 0.05;

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
 * fixed 120 s — on a young backend mostly time before the server began recording, which is
 * `unobserved` (T-507; before T-507 it was served `unknown`, and on this fixture that was 50 of 64
 * rows). Either way it is not the span the pane is showing, so asking over a recent window is how
 * the question "is there data here?" gets an answer about the rows actually on screen — and
 * [[waitForRecordToCover]] makes sure those rows are ones the server could have sampled.
 *
 * **The rows must not be finer than the store's own cells either**: the same 20 s asked as 64 rows
 * (0.3 s each) comes back with 1536 `unknown` cells and as 8 rows (2.5 s each) with none, because a
 * sub-second row falls inside a level-0 cell rather than covering one. Hence `coverage`'s default of
 * 8 rows, and RECENT_S long enough that 8 of them are comfortably coarser than a 1 s cell.
 */
const RECENT_S = 20;
const recent = () => { const now = Date.now() / 1000; return [now - RECENT_S, now]; };

/**
 * **An upper bound on how many seconds a pane spans, read off its own ruler** (T-459): the oldest
 * time tick's age plus the widest gap between ticks. Ticks sit on every multiple of one step inside
 * the window, so the window's older edge lies less than one step past the oldest tick. `null` when
 * the ruler states fewer than two time ticks.
 */
function paneSpanBoundS(ruler) {
  const m = /time (.*)$/.exec(ruler ?? "");
  if (!m) return null;
  const unit = { ms: 1e-3, s: 1, m: 60, h: 3600 };
  const ages = m[1].split(",").map((x) => {
    const mm = /([\d.]+) m ([\d.]+) s/.exec(x);
    if (mm) return Number(mm[1]) * 60 + Number(mm[2]);
    const t = /([\d.]+) (ms|s|h)/.exec(x);
    return t ? Number(t[1]) * unit[t[2]] : NaN;
  }).filter(Number.isFinite).sort((a, b) => a - b);
  if (ages.length < 2) return null;
  let gap = 0;
  for (let i = 1; i < ages.length; i++) gap = Math.max(gap, ages[i] - ages[i - 1]);
  return ages[ages.length - 1] + gap;
}

/**
 * **Wait until this server's record covers both the pane and the comparison window** (T-507):
 * `horizon.recording_began_s` older than the pane's span (bounded from its ruler) and than
 * `RECENT_S`, with a second's margin. Before that, the oldest rows of both are time before the
 * server existed — honestly unobserved, honestly grey — and a claim about "rows the radio sampled"
 * would be measured against rows it could not have sampled. Throws if it never gets there.
 */
async function waitForRecordToCover(page, backend, view, { timeoutMs = 60000 } = {}) {
  const t0 = Date.now();
  for (;;) {
    const ruler = await page.eval(
      `document.querySelector('.hk-surface-viewport[data-viewport="pane"] .hk-surface-ruler')?.textContent ?? ""`);
    const paneS = paneSpanBoundS(ruler);
    assert.ok(paneS !== null, `the pane's ruler states no time extent to bound: ${JSON.stringify(ruler)}`);
    const h = (await get(backend,
      `/api/coverage?f_lo=${Math.round(view.loHz)}&f_hi=${Math.round(view.hiHz)}&cells=1`)).horizon;
    assert.ok(typeof h?.recording_began_s === "number", `the server names no recording_began_s: ${JSON.stringify(h)}`);
    const ageS = Date.now() / 1000 - h.recording_began_s;
    if (ageS > Math.max(paneS, RECENT_S) + 1) return { ageS, paneS, ruler, waitedMs: Date.now() - t0 };
    assert.ok(Date.now() - t0 < timeoutMs,
      `the server's record (${ageS.toFixed(1)} s) never came to cover the pane (${paneS.toFixed(1)} s)`);
    await new Promise((r) => setTimeout(r, 500));
  }
}

/**
 * **Wait until the server reports no UNOBSERVED row in the pane's body** — the part of the pane
 * below the live-edge zone, over the pane's own band, bounded in time by its ruler — and return
 * the answer that said so.
 *
 * The premise of a grey-share baseline is that the rows it measures are rows the radio sampled.
 * A following pane's oldest rows can legitimately predate the server (or sit in a retune's settle
 * gap), and those are honestly grey; they leave the body as the pane follows, within one pane span.
 * So the budget is that span plus a margin, derived from the pane, and a body still unobserved
 * after it is reported with the server's own counts.
 *
 * Rows are ~2 s: coarse enough that no row falls inside one of the store's 1 s cells (see
 * [[RECENT_S]]), fine enough that time before the server began is a whole unobserved row. A partial
 * row (a sub-second settle gap) is `observed` with `duty < 1` and is within the baseline's 5 %.
 */
async function waitForBodyObserved(page, backend, view) {
  const t0 = Date.now();
  let paneS = null, bodyS = 0, last = null, first = null;
  for (;;) {
    const ruler = await page.eval(
      `document.querySelector('.hk-surface-viewport[data-viewport="pane"] .hk-surface-ruler')?.textContent ?? ""`);
    paneS = paneSpanBoundS(ruler);
    assert.ok(paneS !== null, `the pane's ruler states no time extent to bound: ${JSON.stringify(ruler)}`);
    const now = Date.now() / 1000;
    const bodyT1 = now - paneS * LIVE_EDGE_ZONE;
    bodyS = paneS * (1 - LIVE_EDGE_ZONE);
    last = await coverage(backend, view, now - paneS, bodyT1, 16, Math.max(1, Math.floor(bodyS / 2)));
    first ??= last;
    if (last.unobserved === 0 && last.observed > 0) return { waitedMs: Date.now() - t0, paneS, bodyS, first, last };
    assert.ok(Date.now() - t0 < (paneS + 30) * 1000,
      `the server still reports ${last.unobserved}/${last.known} cells UNOBSERVED in the pane's body ` +
      `(${spanOf(view)}, the ${bodyS.toFixed(0)} s below the live-edge zone) after ${(Date.now() - t0) / 1000} s, ` +
      `longer than the ${paneS.toFixed(0)} s the pane spans: rows that old do not age out of a following pane`);
    await new Promise((r) => setTimeout(r, 1000));
  }
}

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

    // ——— LET THE GESTURE'S OWN ENUMERATION REACH THE WIRE BEFORE JUDGING IT (T-690) ———
    //
    // This premise used to be read off `page.requests` the instant the last gesture ended, and
    // **that counts the tile route's service rate, not the gesture**. The client asks through a
    // LIFO queue behind an AIMD operating cap, so the addresses a pan/zoom enumerates go out over
    // however long the route takes to answer the ones before them. Measured on this same file,
    // same gestures, same product, in two runs of the same suite:
    //
    //     ~167 ms/tile, cap 4/4  -> 234 tile requests by the end of the gestures
    //     ~3612 ms/tile, cap 1/4 ->   3 tile requests by the end of the gestures
    //
    // and the second run failed here with "too few to conclude anything" — then left the shared
    // page zoomed out where the aggressive gesture had put it, which failed tests 2 and 3 as well.
    // One volume assumption, three red tests.
    //
    // So the wait is on the COUNT ITSELF, bounded, and the bound is the same 4 the assertion
    // wants: a client that enumerates fewer than four addresses for this gesture still fails,
    // exactly as before, and one that enumerated them but has not been served yet is no longer
    // read as one that never asked. (Surface-nav's T-564 partition, one step earlier: there the
    // question was WHICH late requests to count, here it is whether they have been sent yet.)
    const askedSince = () => page.requests.slice(firstIdx).filter((r) => r.url.includes("/api/tiles"));
    const NEEDED = 4, DRAIN_POLLS = 80, DRAIN_EVERY_MS = 500;
    const tDrain = Date.now();
    let polls = 0;
    for (; polls < DRAIN_POLLS && askedSince().length < NEEDED; polls++) {
      await new Promise((r) => setTimeout(r, DRAIN_EVERY_MS));
      await page.frames(2);
    }
    const tiles = askedSince();
    t.diagnostic(`the gesture's enumeration reached the wire as ${tiles.length} tile request(s)` +
      (polls ? ` after ${polls} drain poll(s) (${((Date.now() - tDrain) / 1000).toFixed(1)} s) — the ` +
        "route was answering slowly enough that the queue had not emptied when the gestures ended" : ""));
    assert.ok(tiles.length >= NEEDED,
      `the gesture put only ${tiles.length} tile requests on the wire, even after ${polls} drain ` +
      `poll(s); too few to conclude anything. This is the client enumerating too little, not the ` +
      "route answering too slowly — the wait above is exactly for the latter.");
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
    // **Like with like (T-507).** This claim is about rows the radio SAMPLED, so the pane must lie
    // wholly after the moment this server began recording — and so must the server window it is
    // compared against. On a young backend it does not: the backend here is ~15 s old at this point
    // and the pane spans ~20 s, so its oldest quarter is time before the server existed. Those rows
    // are genuinely never sampled and are now drawn grey — correctly — where until T-507 they were
    // drawn as the `unknown` hatch and so escaped a grey count. Measured on the run that surfaced it:
    // grey by tenth 0 0 0 0 0 0 0 68 100 90 (newest first), `recording_began_s` 14.8 s ago, the
    // ruler's oldest tick −15 s, and the server's own last-20-s answer 1 of 8 rows unobserved —
    // exactly its one pre-start row. So wait until the record reaches back past both windows; a
    // grey pixel over a sampled row still fails the claim below exactly as before.
    const insideAge = await waitForRecordToCover(page, backend, zi.view);
    t.diagnostic(`recording began ${insideAge.ageS.toFixed(1)} s ago; the pane spans at most ` +
      `${insideAge.paneS.toFixed(1)} s (ruler: ${insideAge.ruler}); waited ${insideAge.waitedMs} ms`);
    const insideRes = await waitForResident(page);
    // **The two facts a flat pane is decided by, read at the same moment as the residency.** A pane
    // can be fully resident, fully observed and still come out a flat fill for two reasons that
    // look identical in a pixel census: the tiles were drawn through the wrong TIER byte, or the
    // dB→colour range collapsed. Neither is visible in `counts`, and without them the census
    // assertion below can only say "not a render" and leave the next person to guess which.
    const insideTier = (await pane0(page)).level;
    const insideRange = await page.eval(`(document.querySelector('.sf-range')?.textContent ?? '')`);
    t.diagnostic(`INSIDE residency after ${insideRes.ms} ms: ${insideRes.counts} · ` +
      `${insideTier} · range "${insideRange}"`);
    // **Re-read the pane's box HERE, not before the waits above.** `g` was taken right after the
    // navigation, and `waitForRecordToCover` + `waitForResident` can sit for fifteen seconds; the
    // chrome's own height is not constant across that (T-505 put the tier inside every viewport
    // row's level cell, so a row can wrap and un-wrap as the level changes), and the canvas moves
    // with it. Sampling the stale rectangle reads the page AROUND the pane, which is a flat fill —
    // and produces exactly the signature that sent two branches back: 0 % grey, 0 % magenta, tiles
    // resident, tier and range correct, and "2 distinct, dominant 99 %". It is the T-487 mistake
    // spelled with pixels instead of with time. The wide leg below already re-reads; this one did
    // not, and the asymmetry was the whole defect.
    const movedBy = Math.abs((await paneGeometry(page)).rect.y - g.rect.y);
    g = await paneGeometry(page);
    if (movedBy > 0) t.diagnostic(`the canvas moved ${movedBy} px while the waits above ran — ` +
      "the rectangle sampled below is re-read for exactly this reason");
    const insideG = await sampleGrey(page, bodyRect(g.pane, LIVE_EDGE_ZONE));
    const insideEdge = await sampleGrey(page, bodyRect(g.pane, 0.06, LIVE_EDGE_ZONE), { n: 3, gapMs: 700 });
    const insidePix = insideG.last;
    const { cov: insideCov, cellHz: insideCellHz, n: insideN } = await atPaneLevel(zi.view);
    t.diagnostic(`pane level: ${insideN} cells of ${(insideCellHz / 1e3).toFixed(1)} kHz across the viewport`);
    t.diagnostic(`INSIDE ${spanOf(zi.view)}: pane ${(insideG.mean * 100).toFixed(1)} % THE grey (mean of ` +
      `${insideG.text}); ` +
      `server (last ${RECENT_S} s) ${(insideCov.unobservedShare * 100).toFixed(1)} % unobserved OF KNOWN ` +
      `(${insideCov.observed} obs / ${insideCov.unobserved} unobs / ${insideCov.unknown} unk of ${insideCov.total}); ` +
      `census ${insidePix.census.distinct} distinct, dominant ${(insidePix.census.dominantShare * 100).toFixed(0)} % ` +
      `(top: ${insidePix.census.top.map(([c, n]) => `${c}x${n}`).join(" ")})`);
    t.diagnostic(`INSIDE grey by vertical tenth over the WHOLE pane (newest first): ` +
      `${insideG.all[insideG.all.length - 1].bandsText}`);
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
    // **Re-read the pane's box AFTER the wait, not before it** — the same asymmetry T-564 fixed in
    // the INSIDE leg above, still present here (T-690). `waitForResident` can sit for twenty-five
    // seconds, and this leg has just changed the pane's LEVEL twenty times: T-505 puts the tier
    // inside every viewport row's level cell, so that row wraps and un-wraps as the level changes
    // and the canvas moves with it. Sampling a rectangle read before the wait reads the page
    // AROUND the pane, which is a flat fill — 0 % grey, tiles resident, and "2 distinct, dominant
    // 99 %". The inside leg carried exactly this bug and it sent two branches back; leaving the
    // other half of the same test to be caught by luck about how long residency took is the same
    // defect waiting for a slower machine.
    const wideBefore = await paneGeometry(page);
    const wideRes = await waitForResident(page);
    g = await paneGeometry(page);
    const wideMovedBy = Math.abs(g.rect.y - wideBefore.rect.y);
    t.diagnostic(`OUTSIDE residency after ${wideRes.ms} ms: ${wideRes.counts}` +
      (wideMovedBy > 0 ? ` — the canvas moved ${wideMovedBy} px while that wait ran, which is why ` +
        "the rectangle sampled below is re-read here" : ""));
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
    // **THE LIVE-EDGE CLAIM** (T-532), asserted rather than reported — see [[LIVE_EDGE_ZONE]] for
    // what it cost to leave this as a diagnostic. Its premise is the same `insideCov` the assertion
    // above already checked: the server says this band is >90 % observed, so any grey in the newest
    // rows is the surface claiming the radio never looked at rows it recorded.
    assert.ok(insideEdge.mean < EDGE_GREY_MAX,
      `${(insideEdge.mean * 100).toFixed(1)} % of the LIVE-EDGE ZONE (newest ${(LIVE_EDGE_ZONE * 100).toFixed(0)} % of ` +
      `a pane over a band the server reports ${(insideCov.observedShare * 100).toFixed(1)} % OBSERVED) is drawn as ` +
      `THE grey (${insideEdge.text}; by vertical tenth over the whole pane, newest first: ${insidePix.bandsText}). ` +
      "The newest rows were recorded, folded and served — a resident tile's coverage plane is evidence only up to " +
      "`coverage.horizon.as_of_s`, and drawing grey past it says the radio never looked at rows it did look at " +
      "(T-532; CLAUDE.md: the live view renders like a classic SDR waterfall).");
    assert.ok(isRender(insidePix.census),
      `the pane over observed spectrum is not a render: ${insidePix.census.distinct} distinct, ` +
      `dominant ${(insidePix.census.dominantShare * 100).toFixed(0)} % ` +
      `(top: ${insidePix.census.top.map(([c, n]) => `${c}x${n}`).join(" ")}) — with ${insideRes.counts}, ` +
      `drawn as ${insideTier}, over range "${insideRange}". Resident and observed and still flat is ` +
      "not a delivery problem, and with those two right it is not a render problem either: check " +
      "that the rectangle sampled is still ON the pane (the canvas moves when the chrome's height " +
      "changes) before looking at the tile route.");
    assert.deepEqual(page.exceptions, [], "uncaught exception during the pan/zoom leg");
  });

/**
 * Put the active pane somewhere a retune really moves the front end, and read the destination the
 * enabled control names. Test 2's positioning, shared with T-508's fault tests (5 and 6) so all three
 * press the same control the same way. Returns the window before the press, the named destination,
 * and the point the gestures were made at.
 */
async function planRetune(page, backend, t) {
  // **Start from a known view, whatever the test before this one left behind** (T-690). The file
  // shares one page across six tests, and test 1's last act is an aggressive zoom-out; when test 1
  // FAILS it never reaches its own `reopen`, so the page is handed on parked on the whole surface
  // — from which `navigate` below cannot get back inside the tuned window, and tests 2 and 3 fail
  // for a reason that is nothing to do with what they test. One red test became three. `reopen` is
  // this file's own documented way back to a known live view (it re-derives the opening window
  // from the observed coverage, rather than re-implementing that arithmetic here), and the
  // init-script observers survive it because they are installed per document.
  await reopen(page, backend);
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
  return { w0, wantCenterHz, wantSpanHz, at };
}

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
  const { w0, wantCenterHz, wantSpanHz, at } = await planRetune(page, backend, t);

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
    // Same reason as `planRetune`'s (T-690): begin from a known view rather than from whatever the
    // previous test left, so a failure there cannot make this one fail about something else.
    await reopen(page, backend);
    // Below the live-edge zone ([[LIVE_EDGE_ZONE]]), which is why the return below is given time to
    // scroll: the rows that arrived while the tile was off screen ARE the newest rows on return, so
    // measuring immediately would put the subject inside the one band whose grey cannot be read.
    //
    // **Re-read at each measurement, never once at the top** (T-690). Between the two censuses
    // below this test spends half a minute in residency waits and settle sleeps, and T-505 puts
    // the tier inside every viewport row's level cell — so the row wraps and un-wraps as the level
    // changes and the canvas moves with it. A rectangle read once names a box on the PAGE, not a
    // box on the PANE, and the two stop being the same thing the moment the chrome reflows: the
    // before/after comparison would then be between two different subjects, or between two
    // measurements of the shell around the pane (which holds no grey at all, so the claim would
    // pass for free).
    const bodyNow = async () => bodyRect((await paneGeometry(page)).pane, LIVE_EDGE_ZONE);
    const at = await centreOf(page);

    // The pane must be FOLLOWING, or there is no live tile here to go stale.
    await goLive(page);
    const row = await pane0(page);
    assert.equal(row.following, true, "the pane is not following the live edge, so this test has no subject");

    // **And it must be OVER DATA, which this test now establishes rather than inherits** (T-690).
    // A reopen puts the pane on the observed extent from the coverage map plus a margin, which
    // after test 2's retune is several times the tuned window — so most of the pane is spectrum
    // this radio never looked at and is correctly grey (measured: 95.4 %), and the baseline this
    // test compares a return against would be gone. It used to inherit test 2's viewport, which is
    // the coupling the reopen above exists to remove; inheriting the premise instead of stating it
    // is the same defect one step along.
    const wTuned = await tunedWindow(backend);
    const inner = { loHz: wTuned.centerHz - wTuned.spanHz * 0.15, hiHz: wTuned.centerHz + wTuned.spanHz * 0.15 };
    // Twice `navigate`'s default step budget, and for a stated reason rather than a nudge: this
    // target is 30 % of the tuned window (test 1's is 60 %), and the reopened view starts on the
    // whole observed extent plus a margin, so it is about twice as many wheel steps of zoom. The
    // budget is a count of gestures, not a duration.
    const into = await navigate(page, at, inner, { steps: 80 });
    t.diagnostic(`into the tuned window in ${into.steps} gesture(s): ${spanOf(into.view)} ⊂ ${spanOf(wTuned)}`);
    assert.ok(into.view.loHz >= wTuned.loHz && into.view.hiHz <= wTuned.hiHz,
      `the viewport never got inside the tuned window: ${spanOf(into.view)} vs ${spanOf(wTuned)} after ` +
      `${into.steps} steps — there is no live tile over data here to lose.\ntrail: ${into.trail.join(" -> ")}`);
    // A pan in frequency can drop a following pane; put it back before the subject is chosen.
    await goLive(page);

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

    // **The control measurement is only a control over rows the radio SAMPLED** — asked of the
    // server, over the pane body's own time window, and waited on. A following pane spans ~80 s
    // here (its time extent is floored at the capture window, not at the server's age) while this
    // server is ~75 s old by now, so the pane's oldest rows are time before recording began:
    // honestly unobserved, honestly grey, draining out of the bottom of the pane one row per row
    // period. Measured alone (deflake-0922): pane 78–106 s against a server 72–75 s old, and a
    // first frame 6.3 % grey that was 0.0 % two frames later; in three gates it was 11.8 %, 22.9 %
    // and 62.3 %, every time in the oldest tenths, and every time read as "no clean baseline".
    // Whether that grey was still on screen was a race between two clocks this test never
    // compared.
    //
    // Why not `waitForRecordToCover`, which test 1 uses for the same premise: it reads
    // `horizon.recording_began_s`, and after test 2's retune that is the HOUR the observation log's
    // first segment is filed under, not the server's first sample (measured: 923 s before a 15 s
    // old server existed) — so it returned at once with the pre-start rows still on screen. The
    // premise is "no row of the body is unobserved", so that is the question asked.
    const pre = await sampleGrey(page, await bodyNow(), { n: 1 });
    const body = await waitForBodyObserved(page, backend, windowOf((await pane0(page)).where));
    t.diagnostic(`before the wait: ${(pre.mean * 100).toFixed(2)} % THE grey, by vertical tenth ` +
      `(newest first) ${pre.last.bandsText}; the server's first answer over the body: ` +
      `${body.first.unobserved}/${body.first.known} cells unobserved`);
    t.diagnostic(`the server reports the pane body observed after ${body.waitedMs} ms: ` +
      `${body.last.observed}/${body.last.known} cells over the ${body.bodyS.toFixed(0)} s below ` +
      `the live-edge zone of a pane spanning at most ${body.paneS.toFixed(0)} s`);

    // ——— on screen: the control measurement ———
    const beforeRes = await waitForResident(page);
    t.diagnostic(`residency before the round trip after ${beforeRes.ms} ms: ${beforeRes.counts}`);
    const beforeG = await sampleGrey(page, await bodyNow(), { n: 4, gapMs: 1500 });
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
    const afterG = await sampleGrey(page, await bodyNow(), { n: 4, gapMs: 1500 });
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
 * +25 s**. That ink is `CELL_MARKS[CELL.UNKNOWN]`, decoded from a well-formed 200: until T-507
 * `hk-api` served every row before the server's first sample as `"unknown"`, so a freshly started
 * server painted its whole past in the fourth state until the window slid past its start. T-507
 * fixed that at the route — time before a server began recording is `"unobserved"` (grey), and
 * `unknown` is only what it recorded and lost. The count below still stands as a guard — if a value
 * ever does reach the ramp and land in that corner of the cube, this is where it shows.
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
  // rows are inside the record, because a row the server recorded and lost (T-507: between
  // `horizon.recording_began_s` and `horizon.oldest_record_s`) is `unknown` coverage, and
  // `unknown` is drawn as a MAGENTA HATCH (T-413/T-423) — a legitimate mark this
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
  //
  // **The boundary is the socket's close, not this process's clock** (T-506's second failure). The
  // instant `backend.stop()` returns is not the instant the page stops being fed, and the gap is
  // not small: the server's last frames are already in the kernel's socket buffer and in Chrome's,
  // and the page dispatches them whenever its event loop next gets there. Measured on this rig
  // under six concurrent e2e runs, over ten kills: the page received rows up to **388 ms after the
  // SIGKILL** (7, 7 and 10 rows in three of the six loaded runs, at the stream's own ~40 ms
  // cadence), and in **every one of the ten** the last row preceded the `close` event — by 21 to
  // 63 ms, one to two row periods. Nothing was ever produced by the dead server; the drain is a
  // fact about measurement latency, not about the stream.
  //
  // So the earlier version of this assertion — `atMs > tKill`, with `tKill` taken in *this* process
  // and, worse, one CDP round trip *before* the signal (28 ms in an unloaded run, enough for one
  // row that arrived 5 ms BEFORE the kill to be counted after it) — was asking a question the test
  // cannot observe the answer to. It went red on drained frames roughly one run in three under
  // load. What the page CAN observe is its socket closing, and that is the boundary used below:
  // the tap pins it in the close handler, and both measurement windows start after it. The claim
  // is unweakened — arguably stronger, since it now also requires that the client NOTICE.
  const failedBefore = page.requests.filter((r) => r.error !== null).length;
  const uploadsAtKill = await page.eval("window.__hkGl.uploads");
  await page.eval("window.__hkWs.armDeath = true");
  const killedPort = Number(new URL(backend.origin).port);
  const tKill = Date.now();   // nothing awaited between here and the signal
  backend.stop();
  const holder = await holdPort(killedPort);
  t.after(() => holder?.close());
  t.diagnostic(`hk serve killed (SIGKILL) under the live page; port ${killedPort} ` +
    (holder ? "held open by a socket that refuses everything" : "COULD NOT BE HELD"));
  assert.ok(holder,
    `the killed server's port ${killedPort} could not be taken over within the grace period, so nothing ` +
    "here can promise the page is not being fed by somebody else's `hk serve` that bound it instead");

  await page.waitFor("the page's own stream socket to close under the dead server",
    "window.__hkWs.deadAt !== null", { timeoutMs: 20000, everyMs: 25 });
  const deadAt = Number(await page.eval("window.__hkWs.deadAt"));
  const drained = JSON.parse(await page.eval(`JSON.stringify({
    rows: window.__hkWs.rows.filter((r) => r.atMs > ${tKill}).length,
    lastMs: window.__hkWs.rows.length ? window.__hkWs.rows[window.__hkWs.rows.length - 1].atMs : null,
  })`));
  t.diagnostic(`the stream's observed end: the socket closed ${deadAt - tKill} ms after the signal; ` +
    `${drained.rows} row(s) were delivered in between (the last ${deadAt - drained.lastMs} ms before ` +
    "the close) — frames already on the wire when the server died, which is why the boundary below " +
    "is the close and not the clock");

  // **The pane's box, re-read now that the page has reacted to the death** (T-690). `body` above
  // was measured while the server was alive; the kill changes what the chrome says, T-505 puts the
  // tier inside the level cell, and a row that wraps moves the canvas under it. Both windows below
  // are shot after this point, so one read here covers both — and a stale box here does not go
  // red, it goes quietly GREEN: the shell around the pane holds no magenta and does not repaint,
  // so both claims would pass without ever looking at the pane.
  const deadBody = bodyRect((await paneGeometry(page)).pane, 0.06, 0.60);
  if (deadBody.y !== body.y || deadBody.h !== body.h) {
    t.diagnostic(`the canvas moved when the stream died (pane body y ${body.y}->${deadBody.y}, ` +
      `h ${body.h}->${deadBody.h}); the two windows below are measured over the box as it is NOW`);
  }

  // Two successive windows, both wholly AFTER the stream's observed end. The first is the
  // legitimate reaction — in-flight requests fail, the client notices, the page says so. The
  // second is whether it SETTLED.
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
  //
  // The window is the two measurement windows above — the same ten seconds test 2 counts over, so
  // the two numbers are directly comparable — and it begins at the stream's OBSERVED end, so every
  // frame the dead server had already put on the wire is on the other side of the boundary. A row
  // after the close is what this asks about, and there is no benign way to produce one: the socket
  // is shut, so the byte would have to come from somewhere the page is not listening to.
  const wsAfterKill = JSON.parse(await page.eval(`JSON.stringify({
    total: window.__hkWs.rows.length,
    afterClose: window.__hkWs.rows.filter((r) => r.atMs > window.__hkWs.deadAt).length,
    beforeKill: window.__hkWs.rows.filter((r) => r.atMs > ${tBeforeKill} && r.atMs <= window.__hkWs.deadAt).length,
    reopened: window.__hkWs.sockets.filter((s) => s.readyState === 1).length,
  })`));
  const sinceDead = ((Date.now() - deadAt) / 1000).toFixed(0);
  t.diagnostic(`the tap across the kill: ${wsAfterKill.beforeKill} rows in the seconds BEFORE the stream ` +
    `closed (from ${rowsBeforeKill} total), ${wsAfterKill.afterClose} in the ${sinceDead} s after it ` +
    `closed; ${wsAfterKill.reopened} socket(s) open now`);
  assert.equal(wsAfterKill.afterClose, 0,
    `${wsAfterKill.afterClose} spectrum rows reached the page in the ${sinceDead} s AFTER its stream socket ` +
    `closed on a killed server — and port ${killedPort} was held throughout by a socket that refuses every ` +
    "connection, so they cannot have come from another run's `hk serve` taking it over. The tap test 2 " +
    "relies on does not go to zero when the stream dies, and test 2's ≥ 20 rows in ten seconds would " +
    "then prove nothing");
  assert.ok(wsAfterKill.beforeKill > 0,
    "the tap recorded no rows in the seconds before the kill either, so it was not observing a live " +
    "stream and the zero above says nothing");

  const pixW2 = inspect(imgW2, deadBody);
  const churn = diff(imgW1, imgW2, deadBody);
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

// ===========================================================================
// 5. A RETUNE THE DEVICE REFUSES: capture comes back, it does not quietly end
// ===========================================================================
//
// **T-508.** Test 2 was green over the user's "retune kills the live view; it never recovers"
// (reported three times) because the mock SDR always landed exactly where it was told. This one runs
// the same press against a mock with a FAULT: `retune-apply-fails:1` refuses the first retune the
// way a HackRF does — the control call is accepted, and the capture thread's read that applies it
// fails (`hackrf.rs` applies controls at a block boundary on the capture thread), leaving the front
// end where it was.
//
// A property of **the backend's own run state and of rows on the wire after the press**: the run is
// not finished, capture reports `running`, the front end reaches the destination the control named
// (the refusal was transient, so the retry lands), and live rows keep arriving at the new centre.
//
// ——— RED WITHOUT THE FIX, MEASURED (2026-09-18) ———
// Against an `hk` built from `main` plus only the mock fault (`main`'s UI too, via HK_BIN and
// HK_E2E_UI_DIST), this fails 5.6 s in on `run.finished`: "the run ENDED after a retune the device
// refused once … true !== false". The press was answered OK, the device refused the change on the
// new segment's first read, and the supervisor took the capture thread's error for the end of the
// run. **Note what did NOT catch it:** `/api/navigation` went on reporting the destination, so the
// "front end reaches the destination" check — test 2's check — passed on the dead run. That is why
// the run-state assertion comes first.
test("5. a retune the device refuses restarts capture: rows resume and the run does not end", async (t) => {
  const { page, browser, backend } = await open({ port: PORT_BASE + 2, mockFault: "retune-apply-fails:1" });
  try {
    assert.match(backend.log(), /mock SDR: fault armed from HK_MOCK_FAULT/,
      "the harness asked for a device fault and the server did not arm one, so this would be test 2 again");
    const { w0, wantCenterHz } = await planRetune(page, backend, t);

    // Press, and read where the FRONT END is — as test 2 does, including its re-press: a press can
    // legitimately land inside a re-plumb (or, here, a recovery) and come back busy.
    //
    // ——— T-529: WHY THIS TEST IS DETERMINISTIC ———
    // It was not. `retune-apply-fails:1` arms ONE refusal, and the press used to commit the retune
    // as TWO device requests ~1.5 ms apart: `/api/control/rate` (which re-plumbs) and then
    // `/api/control/center`. The fault fires on the capture thread of whichever change reaches
    // `apply_pending` first, and the second request's re-plumb — already queued by then — made the
    // supervisor take its request-first branch, where the failure was counted as neither a capture
    // failure nor a recovery. So `capture_failures` read 0 and the premise below failed, twice in
    // five runs under load, with nothing wrong. The fix is not here and is not a retry: one user
    // retune is now ONE device action (`POST /api/control/window`), so there is exactly one change
    // for the one armed fault to land on. `deviceRequests` asserts that property directly, so a
    // client that splits the retune again fails this test loudly instead of flaking it.
    const deviceIdx = page.requests.length;
    const DEVICE_ROUTE = /\/api\/control\/(center|rate|window|gains|bias_tee|baseband_filter)$/;
    let presses = 0;
    let w1 = w0;
    for (let attempt = 0; attempt < 6; attempt++) {
      presses++;
      await page.click(`document.querySelector('${PANE_ACTION}')`);
      await page.frames(4);
      for (let i = 0; i < 24; i++) {
        w1 = await tunedWindow(backend);
        if (Math.abs(w1.centerHz - wantCenterHz) < 100) break;
        await new Promise((r) => setTimeout(r, 250));
      }
      if (Math.abs(w1.centerHz - wantCenterHz) < 100) break;
      t.diagnostic(`press ${attempt + 1}: front end at ${MHz(w1.centerHz)} MHz; toast: ${await page.$text("#toast")}`);
    }
    const deviceReqs = page.requests.slice(deviceIdx)
      .filter((r) => DEVICE_ROUTE.test(new URL(r.url).pathname));

    // **Wait for the refusal, do not assume the counter has caught up** (T-529). The press loop
    // above exits as soon as the front end reports the destination — and it reports it as soon as
    // the control plane COMMANDS it, which is before the new segment has read a single block. The
    // device's refusal happens on that first read (`apply_pending` on the capture thread, exactly
    // as the HackRF driver applies controls), so reading `capture_failures` at command time is
    // reading it too early. Under the old two-post client the second post tuned the RUNNING segment
    // in place, so the fault landed while the loop was still polling and the counter usually had
    // moved by the time it was read — "usually" being the other half of the flake.
    //
    // This is a wait for an effect that is now GUARANTEED, not a retry of the action: one command
    // that moves the window, one armed fault, so exactly one refusal must occur. Nothing is
    // re-pressed, and the assertion below is unchanged (`>= 1`); if the fault never fires this
    // still fails, 20 s later.
    let stats = {};
    for (const deadline = Date.now() + 20000; Date.now() < deadline;) {
      stats = (await get(backend, "/api/status")).control?.stats ?? {};
      if ((stats.capture_failures ?? 0) >= 1) break;
      await new Promise((r) => setTimeout(r, 250));
    }
    const st = (await get(backend, "/api/control/state")).run;
    t.diagnostic(`after ${presses} press(es): run finished=${st.finished} capture=${st.capture} segment=${st.segment}; ` +
      `capture_failures=${stats.capture_failures} capture_recoveries=${stats.capture_recoveries}; ` +
      `device requests ${JSON.stringify(deviceReqs.map((r) => new URL(r.url).pathname))}`);
    // T-529, and the reason the premise below is now a fact rather than a coin toss: **no press
    // produces more than one device request**, and the one it produces carries the whole window.
    // Bounded by `presses` rather than equal to it: a press onto a momentarily disabled control is
    // a legitimate no-op (the loop re-presses), and the claim here is about splitting, not about
    // how many of the six attempts landed.
    assert.ok(deviceReqs.length >= 1 && deviceReqs.length <= presses,
      `${presses} press(es) produced ${deviceReqs.length} device requests ` +
      `(${JSON.stringify(deviceReqs.map((r) => new URL(r.url).pathname))}). One user retune is one device ` +
      "action; more than one per press means the armed fault can land on either, which is the flake this test had.");
    assert.deepEqual([...new Set(deviceReqs.map((r) => new URL(r.url).pathname))], ["/api/control/window"],
      `a press reached a device route other than the whole-window one: ${JSON.stringify(deviceReqs.map((r) => r.url))}`);
    assert.equal(st.finished, false,
      "the run ENDED after a retune the device refused once, while hk serve kept answering. This is T-508: " +
      "a recoverable failure must restart capture, not end the run.");
    assert.ok(Math.abs(w1.centerHz - wantCenterHz) < 100,
      `the front end is at ${MHz(w1.centerHz)} MHz and the control said ${MHz(wantCenterHz)} MHz ` +
      `(toast: ${await page.$text("#toast")}; run ${JSON.stringify({ finished: st.finished, capture: st.capture })})`);
    // The premise, stated after the fact so it can be read on a green run too: the fault DID fire.
    assert.ok(stats.capture_failures >= 1,
      `the device never refused anything (capture_failures ${stats.capture_failures}), so this passed without the fault it is about`);

    const tSettled = await page.eval("Date.now()");
    await new Promise((r) => setTimeout(r, 10000));
    const after = JSON.parse(await page.eval(
      `JSON.stringify(window.__hkWs.rows.filter((r) => r.atMs > ${tSettled}).map((r) => r.centerHz))`));
    const atNew = after.filter((c) => Math.abs(c - w1.centerHz) < 1000).length;
    t.diagnostic(`in the 10 s after the front end got there: ${after.length} rows, ${atNew} at the new centre`);
    assert.ok(atNew >= 20,
      `only ${atNew} rows arrived at the new centre in the 10 s after a refused-then-recovered retune ` +
      `(${after.length} in total): capture did not really come back`);
    const run = (await get(backend, "/api/control/state")).run;
    assert.equal(run.capture, "running", `capture is not running after it recovered: ${JSON.stringify(run)}`);
    assert.equal(await page.eval("document.querySelector('.sf-capture')?.hidden ?? null"), true,
      "the surface still states a capture failure after capture came back — a banner that outlives " +
      "the fault is the same dishonesty as one that never appears");
    assert.deepEqual(page.exceptions, [], "uncaught exception during the refused retune");
  } finally {
    browser.close();
    backend.stop();
  }
});

// ===========================================================================
// 6. A FRONT END THAT IS GONE: the run ends, and THE SURFACE SAYS SO
// ===========================================================================
//
// **T-508's second half.** Some failures cannot be recovered — `gone-on-retune` takes the mock's
// device away mid-retune and every read after it fails, like a HackRF unplugged or wedged on a USB
// stall. Then the run must end, and the silent frozen edge the user kept seeing is the defect: the
// surface has to SAY capture stopped.
//
// A property of **the canvas's own statement, joined to the backend's**: `/api/control/state`
// reports the run ended (the premise), and the surface — over the picture, not in a nine-character
// label in the top bar — states "Capture stopped" with the backend's cause, having first stated that
// it was trying to recover.
//
// ——— RED WITHOUT THE FIX, MEASURED (2026-09-18) ———
// Against `main` plus only the mock fault: the premise holds (`finished: true`) and the claim fails
// — "the run has ENDED (finished: true) and the surface does not say so: null": no `.sf-capture`
// exists, no state was ever stated (`[]`), and the only trace of it was the top bar's "run
// finished" label.
test("6. a front end that is gone ends the run, and the surface says capture stopped", async (t) => {
  const { page, browser, backend } = await open({ port: PORT_BASE + 4, mockFault: "gone-on-retune" });
  try {
    assert.match(backend.log(), /mock SDR: fault armed from HK_MOCK_FAULT/,
      "the harness asked for a device fault and the server did not arm one");
    await planRetune(page, backend, t);
    const banner = `(() => { const e = document.querySelector('.sf-capture');
      return e ? { hidden: e.hidden, state: e.dataset.state ?? null, text: e.textContent } : null; })()`;
    assert.equal((await page.eval(banner))?.hidden ?? true, true,
      "the surface already states a capture failure before anything failed");

    await page.click(`document.querySelector('${PANE_ACTION}')`);
    const seen = new Set();
    let run = null, b = null;
    const deadline = Date.now() + 45000;
    while (Date.now() < deadline) {
      run = (await get(backend, "/api/control/state")).run;
      b = await page.eval(banner);
      if (b && !b.hidden) seen.add(b.state);
      if (run.finished && b?.state === "ended" && !b.hidden) break;
      await new Promise((r) => setTimeout(r, 200));
    }
    t.diagnostic(`backend: finished=${run?.finished} capture=${run?.capture} note=${JSON.stringify(run?.capture_note)}`);
    t.diagnostic(`surface: states seen ${JSON.stringify([...seen])}; last ${JSON.stringify(b)}`);
    await page.shot(path.join(ART, "canvas-journey-capture-stopped.png"));

    // The premise: the run really did end. Without it the claim below is about nothing.
    assert.equal(run?.finished, true, `the run did not end with its device gone: ${JSON.stringify(run)}`);
    // THE CLAIM.
    assert.ok(b && !b.hidden && b.state === "ended",
      `the run has ENDED (finished: true) and the surface does not say so: ${JSON.stringify(b)}. ` +
      "This is T-508: a frozen edge that looks live is exactly what the user kept seeing.");
    assert.match(b.text, /^Capture stopped/, `the surface's statement: ${JSON.stringify(b.text)}`);
    assert.match(b.text, /device has gone|went away/, "the statement carries the backend's own cause");
    assert.equal(run.capture, "ended");
    assert.ok(seen.has("recovering"),
      `the surface went straight to "stopped" without ever stating it was restarting capture: ${JSON.stringify([...seen])}`);
    assert.equal(await page.$text("#device-label"), "capture stopped", "the top bar agrees with the surface");
    assert.deepEqual(page.exceptions, [], "uncaught exception while capture ended");
  } finally {
    browser.close();
    backend.stop();
  }
});
