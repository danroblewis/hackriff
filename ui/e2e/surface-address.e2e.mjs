// **T-480 in a real browser: what does an aggressive zoom-out actually ASK the route for?**
//
// The user's report was a flooding console: the canvas asked `/api/tiles` for `level_f=10` with
// `t_index=218471` and the route answered `4xx`. `ui/test/surface-address.test.ts` proves the
// derivation cannot leave the lattice, over the whole zoom/pan range — but a unit test cannot see
// what the page in front of the user puts on the wire, and T-450 is the standing proof that "the
// modules are right" and "the page works" are different claims.
//
// ——— WHAT THIS FILE IS A PROPERTY OF, AND WHAT IT IS NOT ———
//
// It reads **every `/api/tiles` request the page made**, off CDP's network events, and separates
// the two things that were being confused when this ticket was written:
//
//   1. **Is the address a node of the lattice the page was told about?** Parsed from the request's
//      own query, compared against the `axes` the server serves. That is T-480's subject and it is
//      asserted at zero, for the product exactly as it ships.
//   2. **Did the route refuse an address that WAS on that lattice?** Reported with the server's own
//      message, because it is a different fault with a different owner: `GET /api/tiles` names a
//      12 x 15 grid of view nodes and the store behind it can only back part of that grid, so a
//      declared node can still be unservable. **A client cannot clamp to a bound nobody states.**
//
// Reporting (2) rather than asserting it is the honest shape: asserting zero 4xx outright would
// fail this file for a defect it does not own, and folding (2) into (1) would be exactly the "sound
// proof of the adjacent question" this repo keeps catching. The second test then closes the loop by
// **stating the missing bound** and showing the client needs nothing else — see its own header.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

const STATUS = `(document.querySelector('[data-slot="status"]')?.textContent ?? '')`;
const READOUT = `[...document.querySelectorAll('.hk-surface-viewport')].map((v) =>
  v.querySelector('.hk-surface-level')?.textContent ?? '').join(' | ')`;

/** The route's own addressable bounds, as `hk-api/src/tiles.rs` states them in its refusals. */
const ADDRESSABLE_HZ = 1e12, ADDRESSABLE_NS = 4_611_686_018_427_387_904;

/**
 * Ask the route, retrying its ingest backpressure.
 *
 * `/api/tiles` answers `503` over `cost.in_flight_limit` concurrent reads (T-454), and this file's
 * own browser holds reads in flight — so a bare `fetch` here would manufacture the refusal and then
 * read it as an answer about the address. A `503` is "busy now", never "no".
 */
async function ask(path, { tries = 40, waitMs = 150 } = {}) {
  for (let i = 0; ; i++) {
    const r = await fetch(`${ORIGIN}${path}`, { headers: { Authorization: `Bearer ${TOKEN}` } });
    if (r.status !== 503 || i >= tries) return r;
    await new Promise((res) => setTimeout(res, waitMs));
  }
}

/** Pull the address out of a request the client built — the request, not the response (T-367). */
function addrOf(url) {
  const q = new URL(url).searchParams;
  const n = (k, d) => (q.has(k) ? Number(q.get(k)) : d);
  return { levelF: n("level_f"), levelT: n("level_t"), fIndex: n("f_index"), tIndex: n("t_index"), cells: n("cells", 256) };
}

/**
 * Open the surface, zoom out as hard as the gestures allow, pan to every corner at that zoom, and
 * hand back every `/api/tiles` request the page made plus the lattice it was told about.
 *
 * Every assertion about movement lives here so neither caller can be vacuous: the level readout
 * must have changed, and the page must have asked for tiles. **The readout is compared for
 * inequality only, never for a value** — a following pane's readout carries its offset from the
 * live edge and that offset drifts with wall-clock lag (T-478), so an `equal` on it goes red under
 * load and a `notEqual` could go green on drift alone. Here the compared field is the *level*
 * string, which no clock touches.
 */
async function zoomOutHard(t, browser, { initScript = null, caps = null } = {}) {
  // The lattice, read off the SERVER rather than restated here: a test that compared the client
  // against its own copy of the number would agree with the copy.
  const probe = await ask("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8");
  assert.equal(probe.status, 200, "the lattice probe must answer, or there is no lattice to test against");
  const axes = (await probe.json()).axes;
  const lat = { f0Hz: axes.frequency.cell_hz, t0Ns: axes.time.cell_s * 1e9 };
  // The ceiling the PAGE was given: the server's declaration, unless the caller stated one for it.
  const capF = caps ? caps.capF : (axes.frequency.max_level ?? axes.frequency.levels - 1);
  const capT = caps ? caps.capT : (axes.time.max_level ?? axes.time.levels - 1);
  t.diagnostic(`the page's lattice: level_f 0..${capF}, level_t 0..${capT} `
    + `(route declares ${axes.frequency.levels} x ${axes.time.levels} levels, cell ${lat.f0Hz} Hz x ${axes.time.cell_s} s)`);

  const page = await browser.page(undefined, initScript ? { initScript } : undefined);
  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });

  const rect = await page.$rect('[data-slot="canvas"]');
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };
  const before = await page.eval(READOUT);
  const firstIdx = page.requests.length;

  // Zoom out hard, on both axes together and on each axis alone, well past any bound either has.
  // 30 notches is ~800x on a 1.25x wheel: whatever the opening window was, this ends at the whole
  // surface on both axes.
  for (let i = 0; i < 30; i++) {
    await page.wheel(mid, 120);
    if (i % 3 === 0) await page.wheel(mid, 120, { shift: true });
    if (i % 3 === 1) await page.wheel(mid, 120, { alt: true });
    if (i % 5 === 0) await page.frames(2);
  }
  await page.frames(6);
  // …then pan to each corner at that zoom, because an INDEX bound is reached by panning, not by
  // zooming, and the two halves of the clamp fail independently.
  for (const [dx, dy] of [[rect.w, 0], [-2 * rect.w, 0], [0, rect.h], [0, -2 * rect.h]]) {
    await page.drag(mid, { x: mid.x + dx * 0.4, y: mid.y + dy * 0.4 }, 6);
    await page.frames(3);
  }
  await page.frames(10);

  const after = await page.eval(READOUT);
  assert.notEqual(after, before, "the gestures must have actually moved the view, or this proves nothing");
  t.diagnostic(`level readout: "${before}" -> "${after}"`);

  const tiles = page.requests.slice(firstIdx).filter((r) => r.url.includes("/api/tiles"));
  const everyTile = page.requests.filter((r) => r.url.includes("/api/tiles"));
  // Non-vacuity, twice over: the gestures must have moved the view (asserted above, from the
  // chrome) and put addresses on the wire, and the page's addressing must have reached the COARSE
  // end of the frequency axis — which is where the reported defect lives. A run that stopped short
  // of the ceiling would pass every assertion below and prove nothing.
  assert.ok(tiles.length >= 4, `the zoom-out must have produced tile requests (got ${tiles.length})`);

  // **Why the ceiling is measured over the page's whole life and not over the gesture alone**
  // (T-460/T-479). `capF` is only ever reached by the parent PIN — one level above what any
  // viewport draws — and on this backend that node is refused **400**, permanently (T-482: the
  // route declares a lattice it cannot serve). A client that asks a permanently-refused address
  // once and never again therefore reaches `capF` at load and never inside the gesture window.
  // Measured on the same backend, same gestures, same two distinct level-11 addresses:
  //
  //   before T-479's terminal-failure rule : 19 requests for those 2 addresses (9 at load, 14 in
  //                                          the gesture), answered 400 — and 503, because the
  //                                          retries were stealing the route's shared in-flight
  //                                          budget from tiles someone was waiting for.
  //   after                                : 2 requests, each address asked exactly once, 400.
  //
  // So the gesture-scoped form of this premise was satisfied by the RETRY STORM rather than by the
  // addressing, and it would go red for anyone who fixed that storm. Counting distinct addressing
  // over the page's whole life asks the question the premise is actually about — *did the client's
  // addressing reach the ceiling* — and, unlike the old form, cannot be satisfied by repetition.
  const coarsest = Math.max(...everyTile.map((r) => addrOf(r.url).levelF));
  assert.equal(coarsest, capF, "the page's addressing must actually reach the top of the frequency axis");

  // **And no third check over the gesture's own wire traffic.** The obvious one — "the gesture
  // asked for the coarsest level it says it drew" — is cache-sensitive in exactly the way the old
  // premise was: a coarse tile resident since load is drawn every frame and requested never, so it
  // passes or fails on how warm the server was, which is how it behaved when tried (green alone,
  // red in the full suite). "The gestures moved the view" is asserted above, from the chrome, and
  // that is the cache-independent form of the same claim.
  const gestureCoarsest = Math.max(...tiles.map((r) => addrOf(r.url).levelF));
  t.diagnostic(`${tiles.length} gesture tile requests (coarsest level_f ${gestureCoarsest}); ` +
    `${everyTile.length} over the page's life, coarsest level_f = ${coarsest}`);
  return { tiles, lat, capF, capT };
}

/** Every request that named a node outside the lattice the page was handed. */
function offLattice({ tiles, lat, capF, capT }) {
  const out = [];
  for (const r of tiles) {
    const a = addrOf(r.url);
    const fTile = lat.f0Hz * 2 ** a.levelF * a.cells, tTile = lat.t0Ns * 2 ** a.levelT * a.cells;
    const bad =
      !(Number.isSafeInteger(a.levelF) && a.levelF >= 0 && a.levelF <= capF) ||
      !(Number.isSafeInteger(a.levelT) && a.levelT >= 0 && a.levelT <= capT) ||
      !(Number.isSafeInteger(a.fIndex) && a.fIndex >= 0 && (a.fIndex + 1) * fTile <= ADDRESSABLE_HZ) ||
      !(Number.isSafeInteger(a.tIndex) && a.tIndex >= 0 && (a.tIndex + 1) * tTile <= ADDRESSABLE_NS);
    if (bad) out.push(`${JSON.stringify(a)} <- ${r.url}`);
  }
  return out;
}

test("an aggressive zoom-out never addresses a node outside the lattice the page was given", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const run = await zoomOutHard(t, browser);

  const off = offLattice(run);
  assert.deepEqual(off.slice(0, 5), [],
    `${off.length}/${run.tiles.length} requests named a node outside the declared lattice`);

  // The residual, in the server's own words. Not asserted — see this file's header for why.
  const refused = run.tiles.filter((r) => r.status >= 400 && r.status < 500);
  if (refused.length) {
    const first = refused[0];
    const body = await (await ask(new URL(first.url).pathname + new URL(first.url).search)).json().catch(() => ({}));
    t.diagnostic(`RESIDUAL: ${refused.length}/${run.tiles.length} ON-LATTICE requests were refused ${first.status}`);
    t.diagnostic(`  first refused address: ${JSON.stringify(addrOf(first.url))}`);
    t.diagnostic(`  the route's own reason: ${body.error ?? "(no body)"}`);
  } else {
    t.diagnostic(`0/${run.tiles.length} tile requests were refused`);
  }
});

test("told the ceiling it cannot infer, the same zoom-out produces ZERO 4xx", async (t) => {
  // ——— WHAT THIS TEST IS A PROPERTY OF ———
  //
  // It is **conditional, and the condition is stated in the test rather than assumed**: *given* the
  // route declares how far up each axis it can be read, the client's address math needs nothing
  // else to stop asking unanswerable questions. It is not evidence that the shipped server is
  // fixed — the test above measures that, and reports the residual.
  //
  // The premise is installed the smallest way there is: one field per axis added to the tile
  // probe's `axes`, which is where `latticeFrom` already looks for it (`max_level`). Nothing else
  // about the page, the gestures or the assertions changes, so what the two tests differ by is
  // exactly one number per axis.
  //
  // The numbers are MEASURED, not chosen: they are the coarsest level each axis answers `200` at
  // with the other axis at 0, probed from this backend below, so they describe this server rather
  // than a number that happened to make the test pass.
  const at = async (lf, lt) => (await ask(`/api/tiles?level_f=${lf}&level_t=${lt}&f_index=0&t_index=0`)).status;
  const probe = await (await ask("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8")).json();
  let capF = 0, capT = 0;
  for (let l = probe.axes.frequency.levels - 1; l >= 0; l--) if (await at(l, 0) === 200) { capF = l; break; }
  for (let l = probe.axes.time.levels - 1; l >= 0; l--) if (await at(0, l) === 200) { capT = l; break; }
  t.diagnostic(`measured servable ceiling on THIS backend: level_f 0..${capF}, level_t 0..${capT} `
    + `(the route DECLARES 0..${probe.axes.frequency.levels - 1} and 0..${probe.axes.time.levels - 1})`);
  assert.ok(capF < probe.axes.frequency.levels - 1 || capT < probe.axes.time.levels - 1,
    "if the route could serve every level it declares there would be nothing here to state, and the "
    + "test above would already be at zero — this premise must be doing work");

  // The premise, as a fetch shim over the tile probe only: `axes.*.max_level`, which is a field the
  // client already reads and the route does not yet send.
  const initScript = `(() => {
    const real = window.fetch;
    window.fetch = async (input, init) => {
      const r = await real(input, init);
      const url = typeof input === "string" ? input : input.url;
      if (!url.includes("/api/tiles") || !r.ok) return r;
      const body = await r.clone().json().catch(() => null);
      if (!body || !body.axes) return r;
      body.axes.frequency.max_level = ${capF};
      body.axes.time.max_level = ${capT};
      return new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
    };
  })();`;

  const browser = await Browser.open();
  t.after(() => browser.close());
  const run = await zoomOutHard(t, browser, { initScript, caps: { capF, capT } });

  // The addresses respect the STATED ceiling, which is strictly tighter than the declared axis —
  // so this is not the previous test restated.
  assert.deepEqual(offLattice(run).slice(0, 5), []);
  const refused = run.tiles.filter((r) => r.status >= 400 && r.status < 500)
    .map((r) => `${r.status} ${JSON.stringify(addrOf(r.url))}`);
  assert.deepEqual(refused.slice(0, 5), [],
    `${refused.length}/${run.tiles.length} tile requests were refused`);
  t.diagnostic(`0/${run.tiles.length} tile requests refused across the whole zoom-out`);
});
