// ****NOT YET ENABLED — named `.pending.mjs`, so `ui/e2e/run.mjs` (which discovers `*.e2e.mjs`) does
// not run it. Rename to `tile-never-black.e2e.mjs` to enable, and only with a green run pasted into the
// ticket.****
//
// Why it is inert (T-1057, 2026-09-26, measured): the browser tier could not be run in this worker's
// worktree at all. There is no `hk` built here, and the one borrowed from the main checkout's
// `target/debug` (2026-09-25 00:45) is older than the client's `planes=compact` tile request, so EVERY
// spec fails before its subject — the control run of the existing `surface-load.e2e.mjs` on the same
// binary failed with *"the surface refused to mount: planes=\"compact\" is not a plane encoding this
// server serves (json, f16)"*, and `run.mjs` warned *"no observed coverage in 60000 ms"* for the lane.
// So this spec is unverified: it is written, reviewed against the tier's own conventions, and left
// switched off rather than landed red. Enabling it needs one thing only — `cargo build -p hk-cli --bin
// hk` from this base, then `cd ui && node e2e/run.mjs tile-never-black` on a port >= 9216.
//
// **T-1057: a visible tile is never abandoned — in a browser, against the real `hk serve`.**
//
// The user, 2026-09-25: *"Sometimes there are black bars in the waterfall, representing tiles that
// haven't been loaded yet; sometimes those never load. If a tile fails to load at all it should be
// re-requested. It seems like they are getting abandoned. Left alone long enough, all tiles on the
// screen should load. I don't think we should ever see the black tiles."*
//
// `ui/test/surface-tile-never-abandoned.test.ts` proves the rule over the cache and the renderer on a
// fake clock. This is the tier that can say the *page* recovers: a real render loop, a real route, and
// the page's own residency readout as the instrument.
//
// **The fault is injected in the page, not in the harness**, by the technique `live-edge.e2e.mjs`
// test 3 established for T-523's proxy: an `initScript` that wraps `window.fetch` before any of the
// app's own scripts run. The statuses are the ones this ticket is about — `400` *"this tile's level
// cannot be built from the levels below it"* and `500` — which `hk-api` really does answer with and
// which T-479 made permanent. It deliberately does **not** inject a transport-level failure: that arms
// T-499's silence ladder, whose ceiling is 30 s, and this claim is about the per-address ladder, which
// recovers in seconds. (The transport case is `live-edge` test 3's subject and stays there.)
//
// **The claim is the pane's own report** (`PaneReport`, via `.hk-surface-counts`), not the pixels and
// not the request log, for the reason T-523 wrote down: `TileCache` answers `pending` for a place it
// has written off — never grey — so a place wrongly made terminal is a `pending` that never clears,
// however long anyone waits and whatever an upscaled coarse ancestor happens to show over it. With the
// fix reverted the readout wedges at a steady `N tiles · M coarse stand-ins · K pending` with nothing
// on the wire, which `waitWhileWorking` reports as a stall rather than waiting out a deadline.

import test from "node:test";
import assert from "node:assert/strict";
import { Browser, tileAsks, waitWhileWorking } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

/** The pane's residency report — `${tiles} tiles · ${fallbacks} coarse stand-ins · ${pending} pending`. */
const COUNTS = `(() => { const v = document.querySelector('.hk-surface-viewport[data-viewport="pane"]');
  return v ? (v.querySelector('.hk-surface-counts')?.textContent ?? '') : ''; })()`;

const parse = (counts) => {
  const m = /(\d+) tiles · (\d+) coarse stand-in\S* · (\d+) pending/.exec(counts);
  return m ? { tiles: Number(m[1]), stand: Number(m[2]), pending: Number(m[3]) } : null;
};

/**
 * Wrap `window.fetch` so that, while `window.__t1057.on`, a share of `/api/tiles` reads (single and
 * batch alike) is answered with a refusal the route itself can produce.
 *
 * Seeded, so a red run is reproducible, and counted, so the test can refuse to pass on a run where the
 * fault never bit. The batch route is included on purpose: a non-`ok` batch response refuses every
 * address it carried, which is the widest version of this defect.
 */
const inject = `(() => {
  const real = window.fetch.bind(window);
  let x = 0x1057;
  const rand = () => { x ^= x << 13; x >>>= 0; x ^= x >>> 17; x ^= x << 5; x >>>= 0; return x / 0x100000000; };
  window.__t1057 = { on: false, share: 0.3, injected: [] };
  const BODIES = [
    [400, "this tile's level cannot be built from the levels below it"],
    [500, "history store poisoned"],
  ];
  window.fetch = (input, init) => {
    const url = typeof input === "string" ? input : input.url;
    if (window.__t1057.on && url.includes("/api/tiles") && rand() < window.__t1057.share) {
      const [status, error] = BODIES[window.__t1057.injected.length % BODIES.length];
      window.__t1057.injected.push(status + " " + url);
      return Promise.resolve(new Response(JSON.stringify({ error }), {
        status, statusText: "Refused", headers: { "content-type": "application/json" },
      }));
    }
    return real(input, init);
  };
})();`;

test("30 % of tile reads refused, then healthy: the pane converges with NO pending left", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page(undefined, { initScript: inject });
  assert.equal(await page.goto(`${ORIGIN}/#token=${TOKEN}`), "load");
  await page.waitFor("the app's surface to draw",
    `!!document.querySelector('.sf-canvas') && document.querySelector('.sf-canvas').width > 200`,
    { timeoutMs: 60000 });
  await page.waitFor("the chrome to report a viewport",
    `document.querySelectorAll('.hk-surface-viewport[data-viewport="pane"]').length > 0`, { timeoutMs: 30000 });

  // 1. Refuse a third of the tile reads, and wait until enough of them have actually been refused for
  //    the recovery below to be about something. The share is random, so the wait is on the count.
  await page.eval("window.__t1057.on = true");
  const bit = await page.waitForValue("the injected refusals to reach the pane's own lanes",
    "window.__t1057.injected.length", (n) => n >= 6, { timeoutMs: 60000 });
  t.diagnostic(`refused ${bit.value} tile read(s) while the fault was on, after ${bit.ms} ms`);
  assert.ok(bit.ok, `only ${bit.value} tile reads were refused in ${bit.ms} ms — the fault never bit, so ` +
    "nothing below would be a claim about recovering from one");

  // …and a snapshot of the damage, for the diagnostic and to prove the pane was actually hurt.
  const during = parse(await page.eval(COUNTS));
  t.diagnostic(`residency while refusing: ${JSON.stringify(during)}`);

  // 2. The route is healthy again. Every place the pane still draws must be re-requested and served:
  //    that is the whole invariant, and `pending` reaching 0 with tiles in hand is the page saying so.
  await page.eval("window.__t1057.on = false");
  const r = await waitWhileWorking(page, () => page.eval(COUNTS),
    (counts) => { const c = parse(counts); return !!c && c.tiles > 0 && c.pending === 0; },
    { stallMs: 20000, timeoutMs: 120000, openIsWork: true });
  const asks = tileAsks(page.requests);
  const refused = asks.filter((a) => a.status !== null && a.status >= 400).length;
  t.diagnostic(`after the fault: ${r.value} (${r.ms} ms, stalled ${r.stalledMs} ms); ` +
    `${asks.length} tile addresses asked for, ${refused} refused; ` +
    `${await page.eval("window.__t1057.injected.length")} injected in total`);
  assert.ok(r.ok,
    `the pane never cleared its PENDING places after the route recovered: "${r.value}" — ` +
    `stalled ${r.stalledMs} ms with nothing on the wire, which is a visible tile that was abandoned. ` +
    `${asks.length} addresses asked for in all.`);

  // 3. …and it is the same addresses being re-asked rather than the pane having scrolled away from the
  //    problem: at least one address the fault refused was asked for again afterwards.
  const byAddr = new Map();
  for (const a of asks) {
    const k = `${a.levelF}.${a.levelT}.${a.fIndex}.${a.tIndex}`;
    const prev = byAddr.get(k) ?? { asks: 0, refused: 0, served: 0 };
    prev.asks++;
    if (a.status !== null && a.status >= 400) prev.refused++;
    if (a.status === 200) prev.served++;
    byAddr.set(k, prev);
  }
  const recovered = [...byAddr.entries()].filter(([, v]) => v.refused > 0 && v.served > 0);
  t.diagnostic(`addresses refused and later served: ${recovered.length} ` +
    `(${recovered.slice(0, 4).map(([k, v]) => `${k}: ${v.refused} refused, ${v.served} served`).join("; ")})`);
  assert.ok(recovered.length > 0,
    "no address that was refused was ever served afterwards — the pane converged by drawing something " +
    "else, not by re-requesting what was refused, so this run says nothing about the invariant");
});
