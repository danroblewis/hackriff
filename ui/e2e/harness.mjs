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
export async function waitForSurfaceHistory(origin, token, { timeoutMs = 60000 } = {}) {
  const t0 = Date.now();
  const browser = await Browser.open();
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
  static async open(conn, url, { width = 1440, height = 900 } = {}) {
    // No width/height here: `Target.createTarget` only accepts them for a new *window*, and the
    // viewport is set by `Emulation.setDeviceMetricsOverride` below anyway.
    const { targetId } = await conn.send("Target.createTarget", { url: "about:blank" });
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
    await conn.send("Emulation.setDeviceMetricsOverride",
      { width, height, deviceScaleFactor: 1, mobile: false }, sessionId);
    if (url) await p.goto(url);
    return p;
  }

  #open = new Map();

  constructor(conn, sessionId) {
    this.conn = conn; this.sessionId = sessionId;
    this.console = []; this.exceptions = [];
    /** Every request this page made, in order: `{url, status, error, startedMs, endedMs}`. */
    this.requests = [];
    /** Live and peak concurrency, per url predicate name — see `watchConcurrency`. */
    this.watches = [];
  }

  #sent(m) {
    const rec = { id: m.requestId, url: m.request.url, method: m.request.method, status: null, error: null, startedMs: Date.now(), endedMs: null, counted: true };
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
    this.#release(r);
  }
  #done(id, error) {
    const r = this.#open.get(id);
    if (!r) return;
    this.#open.delete(id);
    r.error = error; r.endedMs = Date.now();
    this.#release(r);
  }

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
