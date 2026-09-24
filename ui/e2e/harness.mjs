// The page under the browser tier: navigate it, watch its network, drive its gestures, and read
// back pixels and DOM (T-455).
//
// The standard this tier has to carry up from the unit tier is in the assertions, not the driver:
// T-441 asserts on pixel histograms, T-442 on the requests the client builds, T-443 on the size of
// every submitted quad. So this file's job is to make those three kinds of evidence available from
// a real browser — `census()` over a screenshot, `net` over CDP's `Network` domain, and `$text` /
// `$count` over the live DOM — and **not** to offer a "did it throw" convenience, which is the
// vacuous test the brief warns about.
import { writeFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { connect, kill, launch } from "./cdp.mjs";
import { census, decodePng } from "./png.mjs";

export { census };

/** CDP's modifier bitmask, from names: **Alt 1, Ctrl 2, Meta 4, Shift 8**. One definition, used by
 * both `wheel` and `drag`, so the two gestures cannot disagree about what "shift" is. */
export function modifierBits({ alt = false, ctrl = false, meta = false, shift = false } = {}) {
  return (alt ? 1 : 0) | (ctrl ? 2 : 0) | (meta ? 4 : 0) | (shift ? 8 : 0);
}

/**
 * Reload `/surface.html` until it reports observed coverage, and return what it said.
 *
 * **Why the runner blocks on this before any test opens a page.** `preview.ts` decides the opening
 * viewport ONCE, at load: with observed coverage it opens on the observed region at the lattice's
 * finest levels; without it, on the whole 1 MHz–6 GHz surface at the coarsest. Those are two
 * different tests — in the first, zooming further *in* is legitimately clamped, and in the second,
 * further *out* is. Racing the page against ingest does not make the suite flaky in the ordinary
 * sense; it makes it silently test a different thing each run, which is worse.
 *
 * This is done by **loading the page** rather than by re-deriving its query, because the query is
 * `surfaceBounds(navigation, oldest_record_s, lattice)` — a lattice-snapped box a harness could
 * only approximate, and an approximation that drifted would gate on the wrong condition. Reloading
 * (rather than waiting inside one page) is the point: the probe runs once per load.
 *
 * **It reports, and never throws.** If the page is broken — which is the whole subject of this tier
 * — this function is the first thing to notice, and if it aborted the run here the failure would
 * be attributed to the harness's setup rather than diagnosed by the guard written for it. So it
 * returns `{ ok: false, reason }` and lets `surface-load.e2e.mjs` say what is wrong, in its own
 * words, with the CSP violation and the exception attached.
 */
export async function waitForSurfaceHistory(origin, token, { timeoutMs = 60000, onSpawn } = {}) {
  const t0 = Date.now();
  // T-740: this Chrome is a DIRECT child of the caller (run.mjs), not of any spec — so it is
  // invisible to `killSpecTree`'s spec-tree walk and to the per-spec timeout. `onSpawn`, if given,
  // reaches all the way down to `cdp.mjs`'s `spawn()` call and fires the instant the pid exists —
  // before Chrome forks any of its own helper processes — so a caller that tracks its own children
  // (for a sweep on SIGINT/SIGTERM/next-run-start) can track this one from the start, rather than
  // trusting the `finally` below, which a `process.exit()` mid-await never reaches.
  const browser = await Browser.open({ onSpawn });
  try {
    const page = await browser.page();
    for (;;) {
      await page.goto(`${origin}/surface.html#token=${token}`);
      try {
        await page.waitFor("the surface to finish addressing",
          `!(document.querySelector('[data-slot="note"]')?.textContent ?? "").startsWith("Addressing")
           || !!document.querySelector(".sp-fail")`, { timeoutMs: 15000 });
      } catch { /* still addressing: fall through */ }
      const text = (await page.$text('[data-slot="census"]')) ?? "";
      const observed = Number(text.match(/(\d+) observed/)?.[1] ?? 0);
      if (observed > 0) return { ok: true, observed, text, ms: Date.now() - t0 };
      if (page.exceptions.length) {
        return { ok: false, ms: Date.now() - t0, text,
          reason: `the page threw while loading: ${page.exceptions[0].text.slice(0, 200)}` };
      }
      if (Date.now() - t0 > timeoutMs) {
        return { ok: false, ms: Date.now() - t0, text,
          reason: `no observed coverage in ${timeoutMs} ms (census: "${text || "(empty)"}")` };
      }
      await new Promise((r) => setTimeout(r, 500));
    }
  } finally { browser.close(); }
}

/**
 * **Poll a node-side predicate until it holds, and say what was waited for when it does not.**
 *
 * The counterpart to [[Page.waitFor]] for facts that live in THIS process rather than in the page —
 * a request having come back on `page.requests`, a server route having answered. It exists so that
 * "wait until the thing happened" never has to be spelled as "wait a while and hope": a readiness
 * wait that reads the wall clock decides, on how busy the box is, whether a test measures the
 * product or measures the scheduler.
 *
 * `timeoutMs` is a FAILURE BOUND, never the wait itself: on expiry it throws naming `what`, which is
 * the sentence whoever reads the red line needs.
 */
export async function until(what, ok, { timeoutMs = 30000, everyMs = 200 } = {}) {
  const t0 = Date.now();
  for (;;) {
    if (await ok()) return Date.now() - t0;
    if (Date.now() - t0 > timeoutMs) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}`);
    }
    await new Promise((r) => setTimeout(r, everyMs));
  }
}

/**
 * Narrow a pane rect (page CSS px) to the columns `Page.unoccludedColumns` found clear of foreign
 * chrome (T-801), and say WHICH SHARE of the canvas's width that is, so a spec comparing pixels
 * against a server answer over the viewport can ask the server about the same frequency sub-range
 * the pixels cover (the pane maps frequency linearly across the canvas's own full width — the
 * panels float over the drawing; they do not reframe it). With `unocc` null (no such element) the
 * pane is returned whole, `fracLo = 0`, `fracHi = 1`.
 */
export function clipToUnoccluded(pane, canvasRect, unocc) {
  if (!unocc || !(unocc.w > 0)) return { ...pane, fracLo: 0, fracHi: 1, clipped: 0 };
  const x0 = Math.max(pane.x, unocc.x), x1 = Math.min(pane.x + pane.w, unocc.x + unocc.w);
  const w = Math.max(0, x1 - x0);
  return { ...pane, x: x0, w, fracLo: (x0 - canvasRect.x) / canvasRect.w, fracHi: (x0 + w - canvasRect.x) / canvasRect.w,
    clipped: pane.w - w };
}

/**
 * **Wait for a page-derived report to reach `done`, and keep waiting while the page is still
 * WORKING towards it.**
 *
 * The mechanism behind every "wait for the pane to become resident" in this tier, shared because
 * the *bound* is the thing that was wrong in all of them and the *predicate* is the thing that must
 * stay each file's own.
 *
 * What was wrong: a fixed deadline (25 s, 40 s) is a bet on the tile route's service rate, and this
 * repo has measured that rate moving more than twenty-fold between a quiet box and a full suite —
 * 167 ms a tile with one other spec running, 3612 ms a tile in the whole suite. The same pane, the
 * same product and the same claim then pass or fail on how many other lanes are up, which is the
 * one thing the test is not about.
 *
 * What replaces it is not a longer deadline. It is the difference between a pane that is FILLING
 * and a pane that is STUCK, which the page states plainly: its report changes, or its requests are
 * on the wire. While either is true the page is working and the wait continues; when BOTH have been
 * quiet for `stallMs` the page has finished doing whatever it is going to do, and the caller's
 * assertion judges that. So the defect these waits exist to catch — a place a refusal made terminal,
 * which is a *steady* `0 tiles · N coarse stand-ins · 0 pending` with nothing on the wire — is
 * reported FASTER than the old deadline reported it, not slower.
 *
 * `timeoutMs` remains, as the failure bound of last resort for a page that churns forever.
 */
export async function waitWhileWorking(page, read, done, {
  everyMs = 400, stallMs = 12000, timeoutMs = 180000, busy = (u) => u.includes("/api/tiles"),
} = {}) {
  const t0 = Date.now();
  const wire = () => page.requests.filter((r) => busy(r.url))
    .reduce((n, r) => n + 1 + (r.endedMs !== null ? 1 : 0), 0);
  let value = await read(), lastSeen = JSON.stringify(value), lastWire = wire(), movedAt = Date.now();
  for (;;) {
    if (done(value)) return { ok: true, value, ms: Date.now() - t0, stalledMs: 0 };
    const elapsed = Date.now() - t0;
    if (elapsed > timeoutMs) return { ok: false, value, ms: elapsed, stalledMs: Date.now() - movedAt };
    if (Date.now() - movedAt > stallMs) return { ok: false, value, ms: elapsed, stalledMs: Date.now() - movedAt };
    await new Promise((r) => setTimeout(r, everyMs));
    value = await read();
    const seen = JSON.stringify(value), w = wire();
    if (seen !== lastSeen || w !== lastWire) movedAt = Date.now();
    lastSeen = seen; lastWire = w;
  }
}

/**
 * Every tile ADDRESS a list of requests asked for, one record per address (T-573).
 *
 * A single `GET /api/tiles` names one address in its query and answers it with its own status. A
 * `GET /api/tiles/batch` names many in `addresses=<level_f>.<level_t>.<f_index>.<t_index>,…` and
 * answers each with its own status inside a 200 (read by [[Page]] off the body); an address it
 * listed in `remaining` was not answered at all (`status: null`, like a request still in flight).
 * A batch whose request itself failed passes that status to every address it carried. The events
 * route is not a tile read and is excluded.
 */
export function tileAsks(requests) {
  const out = [];
  for (const r of requests) {
    if (!r.url.includes("/api/tiles") || r.url.includes("/api/tiles/events")) continue;
    const u = new URL(r.url), q = u.searchParams;
    const common = { url: r.url, startedMs: r.startedMs, respondedMs: r.respondedMs ?? null,
      endedMs: r.endedMs, error: r.error,
      scheme: q.get("scheme") ?? "view", device: q.get("device") ?? "any",
      cells: q.has("cells") ? Number(q.get("cells")) : 256 };
    if (u.pathname.endsWith("/api/tiles/batch")) {
      const byAddr = new Map((r.entries ?? []).map((e) => [e.spelling, e.status]));
      for (const sp of (q.get("addresses") ?? "").split(",").filter(Boolean)) {
        const [levelF, levelT, fIndex, tIndex] = sp.split(".").map(Number);
        const status = r.status !== 200 ? r.status : (byAddr.get(sp) ?? null);
        out.push({ ...common, batch: true, spelling: sp, levelF, levelT, fIndex, tIndex, status,
          key: `${common.device}|${common.scheme}|${common.cells}|${sp}` });
      }
    } else {
      const n = (k) => (q.has(k) ? Number(q.get(k)) : NaN);
      const [levelF, levelT, fIndex, tIndex] = ["level_f", "level_t", "f_index", "t_index"].map(n);
      const sp = `${levelF}.${levelT}.${fIndex}.${tIndex}`;
      out.push({ ...common, batch: false, spelling: sp, levelF, levelT, fIndex, tIndex, status: r.status,
        key: `${common.device}|${common.scheme}|${common.cells}|${sp}` });
    }
  }
  return out;
}

/**
 * The most tile ADDRESSES this page ever had outstanding on the wire at once (T-846).
 *
 * [[Page.watchConcurrency]] counts REQUESTS, and since T-573 a request is a batch of up to 64
 * addresses: a client that ignored the route's in-flight cap entirely put all of them in ONE batch
 * and read `peak 1/4`. The cap the client obeys is per address (`TileCache` charges one slot per
 * address, and the route takes one producer slot per address), so this is the like-for-like count.
 *
 * Each address is outstanding from its request's start until the route ANSWERED it — the response
 * line, not the end of the body, for the reason `#release` gives — or until the request failed or
 * was cancelled. A batch's addresses are all outstanding until the batch answers, which is exactly
 * what the route was asked for and what the client's own budget charged. Like the request count, it
 * is a LOWER bound on what the client had queued behind the browser's connection limit.
 */
export function addressPeak(asks) {
  const ev = [];
  for (const a of asks) {
    const end = a.respondedMs ?? a.endedMs ?? Number.POSITIVE_INFINITY;
    ev.push([a.startedMs, 1], [end, -1]);
  }
  // Ends before starts at the same millisecond: a slot released and re-taken in one tick is not two.
  ev.sort((x, y) => x[0] - y[0] || x[1] - y[1]);
  let live = 0, peak = 0;
  for (const [, d] of ev) { live += d; peak = Math.max(peak, live); }
  return peak;
}

export class Browser {
  static async open(opts = {}) {
    const b = await launch(opts);
    const conn = await connect(b.wsUrl);
    return new Browser(b, conn);
  }
  constructor(b, conn) { this.b = b; this.conn = conn; this.exe = b.exe; }
  async page(url, opts) { return Page.open(this.conn, url, opts); }
  close() { this.conn.close(); kill(this.b); }
}

/** One page target, with its console, its exceptions and its network recorded from before load. */
export class Page {
  static async open(conn, url, { width = 1440, height = 900, initScript = null, newWindow = false } = {}) {
    // **`newWindow` is not cosmetic: it decides whether the page ALREADY OPEN keeps rendering**
    // (2026-09-23). A second target in the SAME window becomes the window's active tab, so the
    // first one's `document.visibilityState` flips to `hidden` and Chrome stops delivering it
    // `requestAnimationFrame` — measured here at 26 frames/s before and **0 frames/s after**, with
    // `--disable-background-timer-throttling` and `--disable-renderer-backgrounding` both already
    // set (they govern timers and process priority, not rAF for a hidden page). The surface's tile
    // demand is computed in its render pass (`preview.ts` `start()` → `frame()`), so a spec that
    // opens a second tab has silently stopped the first one's fetching — which is exactly the
    // situation `surface-contention.e2e.mjs` exists to create. In its own window both pages stay
    // `visible` and both keep rendering (measured 26 and 68 frames/s side by side).
    //
    // Default off, so every existing single-page spec opens exactly the target it always did.
    //
    // width/height are only accepted by `Target.createTarget` for a new *window*; the page's own
    // viewport comes from `Emulation.setDeviceMetricsOverride` below either way.
    const { targetId } = await conn.send("Target.createTarget",
      newWindow ? { url: "about:blank", newWindow: true, width, height } : { url: "about:blank" });
    const { sessionId } = await conn.send("Target.attachToTarget", { targetId, flatten: true });
    const p = new Page(conn, sessionId);
    conn.on("Runtime.consoleAPICalled", (m, sid) => {
      if (sid !== sessionId) return;
      p.console.push({
        level: m.type,
        text: m.args.map((a) => (a.value !== undefined ? String(a.value) : a.description ?? a.type)).join(" "),
      });
    });
    conn.on("Runtime.exceptionThrown", (m, sid) => {
      if (sid !== sessionId) return;
      const d = m.exceptionDetails ?? {};
      p.exceptions.push({
        text: d.exception?.description ?? d.text ?? "(unknown)",
        url: d.url ?? "", line: d.lineNumber ?? -1,
      });
    });
    conn.on("Network.requestWillBeSent", (m, sid) => { if (sid === sessionId) p.#sent(m); });
    conn.on("Network.responseReceived", (m, sid) => { if (sid === sessionId) p.#response(m); });
    conn.on("Network.loadingFinished", (m, sid) => { if (sid === sessionId) p.#done(m.requestId, null); });
    conn.on("Network.loadingFailed", (m, sid) => {
      if (sid === sessionId) p.#done(m.requestId, m.canceled ? "canceled" : m.errorText ?? "failed");
    });
    await conn.send("Network.enable", {}, sessionId);
    await conn.send("Runtime.enable", {}, sessionId);
    await conn.send("Page.enable", {}, sessionId);
    // Installed BEFORE any of the page's own scripts, which is the only place it can be: T-450's
    // throw happened during module evaluation, so a listener added afterwards would arrive after
    // the event it exists to catch. `securitypolicyviolation` names the directive that refused,
    // which is a far more specific claim than "an exception appeared".
    await conn.send("Page.addScriptToEvaluateOnNewDocument", {
      source: `window.__cspViolations = [];
        document.addEventListener("securitypolicyviolation", (e) => window.__cspViolations.push({
          directive: e.effectiveDirective || e.violatedDirective,
          blocked: e.blockedURI, source: e.sourceFile, line: e.lineNumber, sample: e.sample,
        }));`,
    }, sessionId);
    // **A test's own instrumentation, installed before the page's scripts** (T-457).
    //
    // What it is for: asserting a rendering against **the bytes the server actually delivered**,
    // without putting a hook in product code. A trace that renders is not a trace showing the
    // current frame — the only way to tell the difference is to observe the delivered frame
    // independently and compare, and the socket is where it is observable. Injected here for the
    // same reason the CSP listener is: it must run before the app opens its own sockets.
    //
    // It may only ever *observe*. A script that changed what the page does would make this tier a
    // test of a page nobody ships.
    if (initScript) await conn.send("Page.addScriptToEvaluateOnNewDocument", { source: initScript }, sessionId);
    await conn.send("Emulation.setDeviceMetricsOverride",
      { width, height, deviceScaleFactor: 1, mobile: false }, sessionId);
    if (url) await p.goto(url);
    return p;
  }

  #open = new Map();

  constructor(conn, sessionId) {
    this.conn = conn; this.sessionId = sessionId;
    this.console = []; this.exceptions = [];
    /** Every request this page made, in order: `{url, status, error, startedMs, respondedMs, endedMs}`. */
    this.requests = [];
    /** Live and peak concurrency, per url predicate name — see `watchConcurrency`. */
    this.watches = [];
  }

  #sent(m) {
    const rec = { id: m.requestId, url: m.request.url, method: m.request.method, status: null, error: null, startedMs: Date.now(), respondedMs: null, endedMs: null, counted: true };
    this.requests.push(rec);
    this.#open.set(m.requestId, rec);
    for (const w of this.watches) if (w.match(rec.url)) { w.live++; w.peak = Math.max(w.peak, w.live); }
  }
  /**
   * Stop counting a request as in flight.
   *
   * Called on the FIRST of `responseReceived` / `loadingFinished` / `loadingFailed`, deliberately:
   * the server releases its own in-flight slot when it answers, so counting a request until its
   * body has finished streaming would overstate concurrency against the very cap it is compared
   * with. The comparison has to be like-for-like, or a client that obeys the cap gets reported as
   * one that does not.
   */
  #release(rec) {
    if (!rec.counted) return;
    rec.counted = false;
    for (const w of this.watches) if (w.match(rec.url)) w.live--;
  }

  #response(m) {
    const r = this.#open.get(m.requestId);
    if (!r) return;
    r.status = m.response.status;
    // **When the SERVER let go of its slot** (T-630), which is not when the body finished
    // streaming. A concurrency measured to `endedMs` would count a request the route has already
    // answered as still holding a slot, and so would report a client that obeys its share as one
    // that does not — the same like-for-like rule `#release` is written for, recorded rather than
    // only acted on so a test can reconstruct the peak per status.
    r.respondedMs = Date.now();
    this.#release(r);
  }
  #done(id, error) {
    const r = this.#open.get(id);
    if (!r) return;
    this.#open.delete(id);
    r.error = error; r.endedMs = Date.now();
    this.#release(r);
    // **A batch's per-address answers live in its BODY** (T-573). `GET /api/tiles/batch` answers
    // 200 for the request and carries each address's own status — a 503, a 400 — inside it, so
    // the status line alone would read every refusal as an answer. Read off CDP after the body
    // landed and reduced to what a test asserts on: which address, and what the route said.
    if (!error && r.status === 200 && r.url.includes("/api/tiles/batch")) {
      const got = this.conn.send("Network.getResponseBody", { requestId: id }, this.sessionId)
        .then(({ body, base64Encoded }) => {
          const text = base64Encoded ? Buffer.from(body, "base64").toString("utf8") : body;
          const j = JSON.parse(text);
          r.entries = (j.tiles ?? []).map((e) => ({ spelling: e.address?.spelling ?? null, status: e.status ?? null }));
          r.remaining = j.remaining ?? [];
        })
        .catch((e) => { r.bodyError = String(e?.message ?? e); });
      this.#bodies.add(got);
      void got.finally(() => this.#bodies.delete(got));
    }
  }

  #bodies = new Set();
  /** Wait until every batch body already requested from CDP has been read into its record. */
  async settleBodies() { await Promise.all([...this.#bodies]); }

  /**
   * Start counting how many requests matching `match` are in flight at once, and keep the peak.
   *
   * This is deliberately measured **on the wire** rather than read out of the client's own
   * in-flight counter: T-454 was a client that already had a cap, an `AbortController` per request
   * and measured cancellation, and still let a `503` reach the user. A test that asks the same
   * bookkeeping whether it is correct would have passed then too.
   */
  watchConcurrency(name, match) {
    const w = { name, match, live: 0, peak: 0 };
    this.watches.push(w);
    return w;
  }

  async goto(url) {
    const loaded = new Promise((res) => {
      const t = setTimeout(() => res("timeout"), 30000);
      this.conn.on("Page.loadEventFired", (_m, sid) => {
        if (sid === this.sessionId) { clearTimeout(t); res("load"); }
      });
    });
    await this.conn.send("Page.navigate", { url }, this.sessionId);
    return loaded;
  }

  /** Evaluate in the page. Throws with the page's own stack when the expression throws. */
  async eval(expression, { awaitPromise = true } = {}) {
    const r = await this.conn.send("Runtime.evaluate",
      { expression, awaitPromise, returnByValue: true }, this.sessionId);
    if (r.exceptionDetails) {
      const d = r.exceptionDetails;
      throw new Error(`page eval threw: ${d.exception?.description ?? d.text}`);
    }
    return r.result.value;
  }

  /**
   * Wait for the preview to finish addressing the surface, and fail immediately — with the card's
   * own words — if it puts up its failure card instead.
   *
   * `preview-main.ts`'s `fail()` replaces the stage and leaves the note reading "Addressing the
   * surface…", so a wait on the note alone turns every abort into a 20-second timeout with no
   * diagnosis. This is the difference between a suite people read and one they learn to ignore.
   */
  async waitForSurfaceMounted({ timeoutMs = 30000 } = {}) {
    await this.waitFor("the surface to finish addressing, or to say why it could not",
      `!(document.querySelector('[data-slot="note"]')?.textContent ?? "").startsWith("Addressing")
       || !!document.querySelector(".sp-fail")`, { timeoutMs });
    const card = await this.$text(".sp-fail");
    if (card !== null) throw new Error(`the surface put up its failure card instead of mounting: ${card}`);
  }

  /** Poll an in-page boolean expression. Every wait in this suite is one of these, named. */
  async waitFor(what, expression, { timeoutMs = 20000, everyMs = 100 } = {}) {
    const t0 = Date.now();
    for (;;) {
      if (await this.eval(`!!(${expression})`)) return Date.now() - t0;
      if (Date.now() - t0 > timeoutMs) {
        throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}\n  expression: ${expression}\n` +
          `  exceptions: ${JSON.stringify(this.exceptions.slice(0, 3))}`);
      }
      await new Promise((r) => setTimeout(r, everyMs));
    }
  }

  /**
   * **Poll an in-page expression for a VALUE until `ok(value)` holds, and report rather than throw.**
   *
   * Three properties [[waitFor]] cannot give a test that has to assert on numbers:
   *
   *  - **One evaluation per sample.** Reading a rectangle in one `eval` and the drawing buffer that
   *    is supposed to match it in another is a race against the page's own layout — the chrome's
   *    height changes when a level label wraps, the canvas moves with it, and the two halves of the
   *    comparison then come from two different layouts. Whatever must be compared is read together.
   *  - **It reports the last sample instead of throwing.** The caller keeps its own assertion, with
   *    its own message and its own number, so a genuine defect still fails as itself rather than as
   *    "the harness timed out".
   *  - **`timeoutMs` is a failure bound, not the wait.** A green run returns the moment the page
   *    agrees with itself; a red one says what it was waiting for and what it last saw.
   */
  async waitForValue(what, expression, ok, { timeoutMs = 30000, everyMs = 150 } = {}) {
    const t0 = Date.now();
    let value = null, polls = 0;
    for (;;) {
      value = await this.eval(expression);
      polls++;
      if (ok(value)) return { value, ok: true, what, ms: Date.now() - t0, polls };
      if (Date.now() - t0 > timeoutMs) return { value, ok: false, what, ms: Date.now() - t0, polls };
      await new Promise((r) => setTimeout(r, everyMs));
    }
  }

  /**
   * **Wait until an in-page value STOPS changing, across real frames.**
   *
   * The readiness a gesture needs. A wheel or a drag is applied by the render loop, not by the
   * dispatch, and how many frames that takes is a function of how busy the box is — so "dispatch,
   * sleep 450 ms, read the readout" asks the machine's load whether the gesture happened. This asks
   * the page: sample the readout across frames until `stable` consecutive samples agree, and hand
   * back the settled value.
   *
   * It settles on **no change**, never on a particular value, so it is equally the right wait before
   * an assertion that the view moved and before one that it did not — neither can be made true by
   * waiting, and both stop being decided by when the sample was taken. Reports rather than throws,
   * for [[waitForValue]]'s reason.
   */
  async waitUntilStill(what, expression, { stable = 3, framesEach = 2, timeoutMs = 15000 } = {}) {
    const t0 = Date.now();
    let last = await this.eval(expression), same = 1, samples = 1;
    for (;;) {
      await this.frames(framesEach);
      const now = await this.eval(expression);
      samples++;
      same = JSON.stringify(now) === JSON.stringify(last) ? same + 1 : 1;
      last = now;
      if (same >= stable) return { value: last, still: true, what, ms: Date.now() - t0, samples };
      if (Date.now() - t0 > timeoutMs) return { value: last, still: false, what, ms: Date.now() - t0, samples };
    }
  }

  async $text(selector) {
    return this.eval(`(document.querySelector(${JSON.stringify(selector)})?.textContent ?? null)`);
  }
  async $count(selector) {
    return this.eval(`document.querySelectorAll(${JSON.stringify(selector)}).length`);
  }
  /** An element's CSS box, in page coordinates. */
  async $rect(selector) {
    return this.eval(`(() => { const e = document.querySelector(${JSON.stringify(selector)});
      if (!e) return null; const r = e.getBoundingClientRect();
      return { x: r.x, y: r.y, w: r.width, h: r.height }; })()`);
  }

  /**
   * **The columns of an element nothing FOREIGN is painted over**, read from the page (T-801).
   *
   * Since MAP-01 the Explore canvas is full-bleed and the app's chrome (the inventory `.side`, the
   * `.focus` panel, the top bar and the dock) floats OVER it by design — Google-Maps geometry. A
   * spec that samples the canvas's whole box therefore also samples those panels' pixels, and a
   * claim about what the surface drew ("this band is THE grey", "the trace starts at the band edge")
   * silently becomes a claim about a panel's background. This asks the browser's own hit test,
   * `elementFromPoint`, at every column of the element's box on `rows` evenly spaced rows between
   * `y0` and `y1` (CSS px, page coordinates; default the whole box): a column is **occluded** if
   * any of those points lands on an element outside the element's own mount (`.surface`, or its
   * parent where there is none) — the surface's own overlays (boxes, labels, the capture banner)
   * are part of what it draws and never count. Nothing about any panel's size is assumed: a panel
   * that moves, collapses (`.focus.is-empty`) or is absent (the harness pages) is simply not hit.
   *
   * Returns the widest contiguous unoccluded run as `{ x, w }` in page CSS px (plus the element's
   * box and the occluded-column count), or `null` if the element is absent.
   */
  async unoccludedColumns(selector, { y0 = null, y1 = null, rows = 7 } = {}) {
    return this.eval(`(() => {
      const e = document.querySelector(${JSON.stringify(selector)});
      if (!e) return null;
      const root = e.closest(".surface") ?? e.parentElement ?? e;
      const r = e.getBoundingClientRect();
      const top = ${y0 === null ? "r.top" : Number(y0)}, bot = ${y1 === null ? "r.bottom" : Number(y1)};
      const n = ${Math.max(1, rows | 0)};
      const ys = [];
      for (let i = 0; i < n; i++) ys.push(Math.min(r.bottom - 0.5, Math.max(r.top + 0.5, top + (bot - top) * (i + 0.5) / n)));
      const x0 = Math.ceil(r.left), x1 = Math.floor(r.right);
      let best = { x: x0, w: 0 }, run = null, occluded = 0;
      for (let x = x0; x < x1; x++) {
        let clear = true;
        for (const y of ys) {
          const hit = document.elementFromPoint(x + 0.5, y);
          if (hit && !root.contains(hit)) { clear = false; break; }
        }
        if (clear) { run = run ?? { x, w: 0 }; run.w++; if (run.w > best.w) best = { ...run }; }
        else { occluded++; run = null; }
      }
      return { x: best.x, w: best.w, occluded, box: { x: r.x, y: r.y, w: r.width, h: r.height } };
    })()`);
  }

  /** A decoded screenshot of the composited page. */
  async shot(saveAs = null) {
    const { data } = await this.conn.send("Page.captureScreenshot", { format: "png" }, this.sessionId);
    const buf = Buffer.from(data, "base64");
    if (saveAs) { mkdirSync(path.dirname(saveAs), { recursive: true }); writeFileSync(saveAs, buf); }
    return decodePng(buf);
  }

  // ——— gestures. Real input events, so the page's own listeners run. ———

  /**
   * One raw mouse event.
   *
   * `modifiers` is CDP's bitmask — **Alt 1, Ctrl 2, Meta 4, Shift 8** — and the browser turns it
   * back into `altKey`/`ctrlKey`/`metaKey`/`shiftKey` on the event the page receives, so a test that
   * cares about a modifier is testing flags the *browser* set rather than ones it wrote itself.
   * `modifierBits` below builds it from names.
   */
  async mouse(type, x, y, { button = "left", buttons = 0, clickCount = 0, deltaX = 0, deltaY = 0, modifiers = 0 } = {}) {
    await this.conn.send("Input.dispatchMouseEvent",
      { type, x, y, button, buttons, clickCount, deltaX, deltaY, modifiers, pointerType: "mouse" }, this.sessionId);
  }

  /**
   * Click an element named by an in-page expression that evaluates to it. A real click at the
   * element's own centre, not `el.click()`: the difference is whether anything is on top of it.
   */
  async click(elementExpr) {
    const at = await this.eval(`(() => { const e = ${elementExpr};
      if (!e) return null; const r = e.getBoundingClientRect();
      return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
    if (!at) throw new Error(`nothing to click for: ${elementExpr}`);
    await this.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: 1 });
    await this.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: 1 });
    return at;
  }

  /**
   * A press–move–release drag, in `steps` intermediate moves, with the modifiers held for **the
   * whole stream** — press, every move and the release — which is what a hand does and what a
   * latched gesture has to be driven with to be tested honestly (T-458).
   */
  async drag(from, to, steps = 8, mods = {}) {
    const modifiers = modifierBits(mods);
    await this.mouse("mousePressed", from.x, from.y, { buttons: 1, clickCount: 1, modifiers });
    for (let i = 1; i <= steps; i++) {
      await this.mouse("mouseMoved",
        from.x + ((to.x - from.x) * i) / steps,
        from.y + ((to.y - from.y) * i) / steps, { buttons: 1, modifiers });
      await new Promise((r) => setTimeout(r, 12));
    }
    await this.mouse("mouseReleased", to.x, to.y, { buttons: 0, clickCount: 1, modifiers });
  }

  /** A double-click at a point, which the page treats as a discrete "go there". */
  async dblclick(at) {
    for (const n of [1, 2]) {
      await this.mouse("mousePressed", at.x, at.y, { buttons: 1, clickCount: n });
      await this.mouse("mouseReleased", at.x, at.y, { buttons: 0, clickCount: n });
    }
  }

  /**
   * A wheel at a point, with real modifier bits (T-456).
   *
   * CDP's `modifiers` is a bitmask — **Alt 1, Ctrl 2, Meta 4, Shift 8** — and the browser turns it
   * back into `altKey`/`ctrlKey`/`metaKey`/`shiftKey` on the `WheelEvent` the page receives. That
   * is the point of dispatching it this way rather than constructing a `WheelEvent` in the page: the
   * flags under test are ones the browser set, not ones the test wrote.
   *
   * **What it still cannot prove.** A CDP event enters at the renderer, so it cannot answer whether
   * the *operating system* would have delivered the gesture at all — macOS's Accessibility zoom
   * consumes ctrl+scroll before any browser sees it. `surface-nav.e2e.mjs` says so where it uses
   * this, rather than letting a green run imply more than it measured.
   */
  async wheel(at, deltaY, { shift = false, alt = false, ctrl = false, meta = false, deltaX = 0 } = {}) {
    const modifiers = modifierBits({ shift, alt, ctrl, meta });
    await this.conn.send("Input.dispatchMouseEvent", {
      type: "mouseWheel", x: at.x, y: at.y, deltaX, deltaY,
      modifiers, pointerType: "mouse",
    }, this.sessionId);
  }

  /**
   * Poll a screenshot until the canvas's own rectangle satisfies `ok(census)`, and return the
   * census that satisfied it.
   *
   * A single screenshot at a fixed moment is the flakiest thing this tier could do — the first
   * tiles arrive asynchronously, so "not drawn yet" and "never draws" look identical in one frame.
   * Polling states the claim the test actually means ("within this long, the canvas becomes a real
   * render") and, when it fails, reports the last census rather than a timeout with no evidence.
   */
  async waitForCanvas(selector, ok, { timeoutMs = 60000, everyMs = 500, saveAs = null } = {}) {
    const t0 = Date.now();
    let last = null, rect = null;
    for (;;) {
      rect = await this.$rect(selector);
      if (rect && rect.w > 0 && rect.h > 0) {
        const img = await this.shot(Date.now() - t0 > timeoutMs ? saveAs : null);
        last = census(img, { x: Math.round(rect.x), y: Math.round(rect.y), w: Math.round(rect.w), h: Math.round(rect.h) });
        if (ok(last)) {
          if (saveAs) await this.shot(saveAs);
          return { census: last, rect, ms: Date.now() - t0 };
        }
      }
      if (Date.now() - t0 > timeoutMs) {
        throw new Error(`the canvas never became a real render in ${timeoutMs} ms.\n` +
          `  last census: ${JSON.stringify(last)}\n  rect: ${JSON.stringify(rect)}`);
      }
      await new Promise((r) => setTimeout(r, everyMs));
    }
  }

  /** Let the page run: `n` animation frames, so assertions land after real renders. */
  async frames(n = 3) {
    await this.eval(`new Promise(r => { let k = ${n};
      const step = () => (--k <= 0 ? r(1) : requestAnimationFrame(step)); requestAnimationFrame(step); })`);
  }
}
