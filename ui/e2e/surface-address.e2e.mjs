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
//   2. **Did the route refuse an address that WAS on that lattice?** Asserted at zero **since
//      T-482**, and this is the half that changed. It used to be only *reported*, with the server's
//      own message, because it was a different fault with a different owner: `GET /api/tiles` named
//      a 12 x 15 grid of view nodes while the store behind it could back only part of that grid, so
//      a declared node could still be unservable, and **a client cannot clamp to a bound nobody
//      states.** T-482 states the bound — `axes.{frequency,time}.max_level`, the box the route can
//      actually be read over — so the client can obey it and the refusals are now a defect this
//      file does own.
//
// The two are still asserted separately, because they still fail for different reasons: (1) is the
// client leaving the lattice, (2) is the route refusing inside it. Folding them together would be
// exactly the "sound proof of the adjacent question" this repo keeps catching.
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
  return { scheme: q.get("scheme") ?? "view", levelF: n("level_f"), levelT: n("level_t"), fIndex: n("f_index"), tIndex: n("t_index"), cells: n("cells", 256) };
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
  // **There are TWO lattices now, and a level index means a different cell on each (T-505).** The
  // canvas picks a tier per viewport, per frame, by the tile budget: the detail tier (`scheme=view`)
  // answers the tuned window and the overview tier (`scheme=overview`, anchored on scheme 1)
  // answers the wide-and-long ones, which on this fixture is the minimap. A single `lat`/`capF`
  // read off the detail probe and applied to every request the page made is then two different
  // mistakes at once: it calls a legitimate overview address off-lattice, and it looks for the
  // coarse end of the frequency axis on a lattice the page stopped using for the coarse end. So
  // each scheme is probed for its own declaration and each request is judged against the lattice
  // it actually names (T-564: partition by kind rather than counting one heap).
  const latticeOf = async (scheme) => {
    const q = `/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8` +
      (scheme === "view" ? "" : `&scheme=${scheme}`);
    const r = await ask(q);
    assert.equal(r.status, 200, `the ${scheme} lattice probe must answer, or there is no lattice to test against`);
    const axes = (await r.json()).axes;
    return {
      f0Hz: axes.frequency.cell_hz, t0Ns: axes.time.cell_s * 1e9,
      capF: axes.frequency.max_level ?? axes.frequency.levels - 1,
      capT: axes.time.max_level ?? axes.time.levels - 1,
      levels: `${axes.frequency.levels} x ${axes.time.levels}`, tCellS: axes.time.cell_s,
    };
  };
  const lattices = { view: await latticeOf("view"), overview: await latticeOf("overview") };
  // The caller may still state the detail tier's ceiling for a page it shimmed.
  if (caps) { lattices.view.capF = caps.capF; lattices.view.capT = caps.capT; }
  const lat = { f0Hz: lattices.view.f0Hz, t0Ns: lattices.view.t0Ns };
  const capF = lattices.view.capF, capT = lattices.view.capT;
  for (const [name, L] of Object.entries(lattices)) {
    t.diagnostic(`the page's ${name} lattice: level_f 0..${L.capF}, level_t 0..${L.capT} `
      + `(route declares ${L.levels} levels, cell ${L.f0Hz} Hz x ${L.tCellS} s)`);
  }

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
  // **Non-vacuity, counted where the gesture's work actually lands.** This used to demand four
  // requests from the gesture window alone. That was a volume assumption, and T-505 invalidated it
  // deliberately: the minimap moved to the overview tier, whose tiles are megahertz wide and
  // minutes tall, so a device-wide zoom-out that used to enumerate hundreds of detail addresses now
  // resolves to a handful — and the pane's own coarse tiles are often already resident from the
  // parent pin. Measured under a shared, aged backend: 2. A threshold raised or lowered to suit
  // that tests nothing, so the premise is stated as what it is actually for — the gesture must have
  // put addresses on the wire ACROSS THE SCHEMES THE PAGE USES — and the movement claim above
  // (from the chrome, cache-independent) carries the rest.
  const schemesUsed = new Set(everyTile.map((r) => addrOf(r.url).scheme));
  assert.ok(tiles.length >= 1,
    `the zoom-out produced no tile requests at all (got ${tiles.length}); the page cannot be ` +
    "addressing anything, so nothing below is a measurement");
  assert.ok(schemesUsed.has("view") && schemesUsed.has("overview"),
    `the page only ever addressed ${[...schemesUsed].join(", ")}. Both tiers must be exercised or ` +
    "this file is testing one lattice and calling it the surface (T-505: the canvas picks a tier " +
    "per viewport by the tile budget, so a run that touched one tier proves nothing about the other).");

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
  //
  // **And it is measured PER SCHEME, because the coarse end moved (T-505).** The page no longer
  // reaches the detail lattice's own `max_level` by zooming out — past the tile budget it switches
  // to the overview tier instead, so the coarse end of the surface is now the top of the OVERVIEW
  // lattice's frequency axis. Comparing every request's `level_f` against the detail lattice's
  // `capF` read 11 against 9 and said "the addressing must reach the top", when what it had found
  // was a level 11 that is perfectly on-lattice for the scheme that named it.
  const coarsestOf = (scheme) => {
    const xs = everyTile.map((r) => addrOf(r.url)).filter((a) => a.scheme === scheme);
    return xs.length ? Math.max(...xs.map((a) => a.levelF)) : null;
  };
  const reach = Object.fromEntries(Object.keys(lattices).map((k) => [k, coarsestOf(k)]));
  t.diagnostic(`coarsest level_f addressed per scheme: ` +
    Object.entries(reach).map(([k, v]) => `${k}=${v}/${lattices[k].capF}`).join(" "));
  assert.equal(reach.overview, lattices.overview.capF,
    `the page's addressing reached level_f ${reach.overview} on the OVERVIEW tier, against that ` +
    `lattice's ceiling of ${lattices.overview.capF}. Since T-505 the coarse end of the surface is ` +
    "the overview lattice's top, not the detail lattice's: a zoom-out past the tile budget switches " +
    "tier rather than climbing further on the fine one. A run that stopped short of this ceiling " +
    "would pass every assertion below and prove nothing about the addressing that was reported.");

  // **And no third check over the gesture's own wire traffic.** The obvious one — "the gesture
  // asked for the coarsest level it says it drew" — is cache-sensitive in exactly the way the old
  // premise was: a coarse tile resident since load is drawn every frame and requested never, so it
  // passes or fails on how warm the server was, which is how it behaved when tried (green alone,
  // red in the full suite). "The gestures moved the view" is asserted above, from the chrome, and
  // that is the cache-independent form of the same claim.
  const perScheme = (rs) => Object.entries(
    rs.map((r) => addrOf(r.url)).reduce((m, a) => {
      m[a.scheme] = Math.max(m[a.scheme] ?? -1, a.levelF); return m;
    }, {})).map(([k, v]) => `${k}:${v}`).join(" ") || "none";
  t.diagnostic(`${tiles.length} gesture tile requests (coarsest level_f by scheme ${perScheme(tiles)}); ` +
    `${everyTile.length} over the page's life, coarsest by scheme ${perScheme(everyTile)}`);
  return { tiles, lat, capF, capT, lattices };
}

/** Every request that named a node outside the lattice the page was handed. */
function offLattice({ tiles, lattices }) {
  const out = [];
  for (const r of tiles) {
    const a = addrOf(r.url);
    // **Judged against the lattice the request NAMES.** A `scheme=overview` address expressed in
    // the detail lattice's cells is a different tile and a different ceiling (T-505), and reading
    // one against the other is how a legitimate level 11 came to look off-lattice.
    const L = lattices[a.scheme];
    if (!L) { out.push(`${JSON.stringify(a)} names a scheme this file has no lattice for <- ${r.url}`); continue; }
    const { capF, capT } = L;
    const fTile = L.f0Hz * 2 ** a.levelF * a.cells, tTile = L.t0Ns * 2 ** a.levelT * a.cells;
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

  // **And no request inside that lattice was refused** (T-482). Before the route declared its
  // readable ceiling this was a diagnostic rather than an assertion, because the flood was the
  // route's fault and not the client's: 117 requests, 107 of them 400. The ceiling is what lets the
  // client ask only answerable questions, and this is where that is measured **with no shim** — the
  // page gets the real server's real declaration and nothing else.
  const refused = run.tiles.filter((r) => r.status >= 400 && r.status < 500);
  if (refused.length) {
    const first = refused[0];
    const body = await (await ask(new URL(first.url).pathname + new URL(first.url).search)).json().catch(() => ({}));
    t.diagnostic(`  first refused address: ${JSON.stringify(addrOf(first.url))}`);
    t.diagnostic(`  the route's own reason: ${body.error ?? "(no body)"}`);
  }
  assert.deepEqual(refused.map((r) => `${r.status} ${JSON.stringify(addrOf(r.url))}`).slice(0, 5), [],
    `${refused.length}/${run.tiles.length} ON-LATTICE tile requests were refused`);
  t.diagnostic(`0/${run.tiles.length} gesture tile requests refused across the whole zoom-out`);
});

test("the route DECLARES the ceiling, and every address inside it answers", async (t) => {
  // ——— WHAT THIS TEST IS A PROPERTY OF ———
  //
  // It is a property of **the served declaration against the served answers**, with no browser and
  // no shim in it. Until T-482 this test installed the missing `max_level` itself, as a fetch shim
  // over the tile probe, and was explicitly *conditional*: "given the route declares how far up it
  // can be read, the client needs nothing else". The route declares it now, so the conditional half
  // is gone and what is left is the part a client depends on — that the declaration is **true**.
  //
  // The ceiling is a BOX (`level_f <= max_f` AND `level_t <= max_t`), so the box is what is walked.
  // The servable set itself is an **area** constraint and larger than any box — `(5, 5)` is
  // servable on the shipped geometry and is outside `(9, 1)` — which is why the walk is of the
  // declaration rather than of the grid: over-claiming is the defect, under-claiming is the cost.
  const probe = await (await ask("/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8")).json();
  const { frequency, time } = probe.axes;
  assert.equal(typeof frequency.max_level, "number", "the route must state a frequency ceiling");
  assert.equal(typeof time.max_level, "number", "the route must state a time ceiling");
  assert.ok(frequency.max_level < frequency.levels - 1 || time.max_level < time.levels - 1,
    "if the route could serve every level it declares there would be nothing here to state, and "
    + "the test above would already have been at zero — this declaration must be doing work");
  t.diagnostic(`declared: level_f 0..${frequency.max_level}, level_t 0..${time.max_level} `
    + `(the lattice NAMES 0..${frequency.levels - 1} and 0..${time.levels - 1})`);

  const refused = [];
  for (let lf = 0; lf <= frequency.max_level; lf++) {
    for (let lt = 0; lt <= time.max_level; lt++) {
      const r = await ask(`/api/tiles?level_f=${lf}&level_t=${lt}&f_index=0&t_index=0`);
      if (r.status !== 200) refused.push(`(${lf},${lt}) -> ${r.status} ${(await r.json().catch(() => ({}))).error ?? ""}`);
    }
  }
  assert.deepEqual(refused.slice(0, 5), [],
    `${refused.length} addresses INSIDE the declared ceiling were refused — a ceiling that still `
    + "refuses is the same defect one notch down");
  t.diagnostic(`${(frequency.max_level + 1) * (time.max_level + 1)} addresses walked, 0 refused`);

  // Tight, not merely safe: one level past the box on either axis is genuinely refused.
  for (const [lf, lt] of [[frequency.max_level + 1, time.max_level], [frequency.max_level, time.max_level + 1]]) {
    const r = await ask(`/api/tiles?level_f=${lf}&level_t=${lt}&f_index=0&t_index=0`);
    assert.ok(r.status >= 400, `(${lf},${lt}) answered ${r.status}, so the ceiling is leaving reach unused`);
  }
});
