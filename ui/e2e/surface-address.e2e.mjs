// **T-480 in a real browser: what does an aggressive zoom-out actually ASK the route for?**
//
// The user's report was a flooding console: the canvas asked `/api/tiles` for `level_f=10` with
// `t_index=218471` and the route answered `4xx`. `ui/test/surface-address.test.ts` proves the
// derivation cannot leave the lattice, over the whole zoom/pan range — but a unit test cannot see
// what the page in front of the user actually puts on the wire, and T-450 is the standing proof
// that "the modules are right" and "the page works" are different claims.
//
// ——— WHAT THIS FILE IS A PROPERTY OF, AND WHAT IT IS NOT ———
//
// It reads **every `/api/tiles` request the page made**, off CDP's network events, and separates
// the two things that were being confused when this ticket was written:
//
//   1. **Is the address a node of the lattice the server declared?** Parsed from the request's own
//      query, compared against the `axes` the server serves. This is T-480's subject, and it is
//      asserted at zero: an address off the lattice is the discretized-navigation invariant broken
//      in the client.
//   2. **Did the route refuse an address that WAS on the lattice?** Reported with the server's own
//      message, because it is a different fault with a different owner: the view lattice names a
//      12 x 15 grid of nodes and the store behind it can only back part of that grid, so a declared
//      node can still be unservable. The client cannot clamp to a bound nobody states.
//
// Reporting (2) rather than asserting it is the honest shape. Asserting zero 4xx outright would
// make this file fail for a defect it does not own and cannot fix, and quietly folding (2) into (1)
// would be exactly the "sound proof of the adjacent question" this repo keeps catching.
import test from "node:test";
import assert from "node:assert/strict";
import { Browser } from "./harness.mjs";

const ORIGIN = process.env.HK_E2E_ORIGIN, TOKEN = process.env.HK_E2E_TOKEN;

const STATUS = `(document.querySelector('[data-slot="status"]')?.textContent ?? '')`;
const READOUT = `[...document.querySelectorAll('.hk-surface-viewport')].map((v) =>
  v.querySelector('.hk-surface-level')?.textContent ?? '').join(' | ')`;

/** The route's own addressable bounds, as `hk-api/src/tiles.rs` states them in its refusals. */
const ADDRESSABLE_HZ = 1e12, ADDRESSABLE_NS = 4_611_686_018_427_387_904;

/** Pull the address out of a request the client built — the request, not the response (T-367). */
function addrOf(url) {
  const q = new URL(url).searchParams;
  const n = (k, d) => (q.has(k) ? Number(q.get(k)) : d);
  return { levelF: n("level_f"), levelT: n("level_t"), fIndex: n("f_index"), tIndex: n("t_index"), cells: n("cells", 256) };
}

test("an aggressive zoom-out never addresses a node outside the lattice the server declared", async (t) => {
  const browser = await Browser.open();
  t.after(() => browser.close());
  const page = await browser.page();

  await page.goto(`${ORIGIN}/surface.html#token=${TOKEN}`);
  await page.waitForSurfaceMounted();
  await page.waitFor("the first tile textures to be uploaded",
    `(${STATUS}.match(/(\\d+) uploads/)?.[1] | 0) > 0`, { timeoutMs: 90000 });

  // The lattice the SERVER declares, read from the server rather than restated here: a test that
  // compared the client against a copy of the number would agree with the copy.
  const probe = await fetch(`${ORIGIN}/api/tiles?level_f=0&level_t=0&f_index=0&t_index=0&cells=8`,
    { headers: { Authorization: `Bearer ${TOKEN}` } });
  assert.equal(probe.status, 200, "the lattice probe must answer, or there is no lattice to test against");
  const axes = (await probe.json()).axes;
  const capF = (axes.frequency.max_level ?? axes.frequency.levels - 1);
  const capT = (axes.time.max_level ?? axes.time.levels - 1);
  t.diagnostic(`server axes: level_f 0..${capF}, level_t 0..${capT} (levels ${axes.frequency.levels} x ${axes.time.levels}, `
    + `cell ${axes.frequency.cell_hz} Hz x ${axes.time.cell_s} s)`);

  const rect = await page.$rect('[data-slot="canvas"]');
  const mid = { x: rect.x + rect.w / 2, y: rect.y + rect.h * 0.35 };
  const before = await page.eval(READOUT);
  const firstIdx = page.requests.length;

  // Zoom out hard, on both axes and on each axis alone, well past any bound either has. 30 notches
  // is ~800x on a 1.25x wheel: whatever the opening window was, this ends at the whole surface.
  for (let i = 0; i < 30; i++) {
    await page.wheel(mid, 120);
    if (i % 3 === 0) await page.wheel(mid, 120, { shift: true });
    if (i % 3 === 1) await page.wheel(mid, 120, { alt: true });
    if (i % 5 === 0) await page.frames(2);
  }
  await page.frames(6);
  // …and then pan to each corner at that zoom, because an index bound is reached by panning, not
  // by zooming.
  for (const [dx, dy] of [[rect.w, 0], [-2 * rect.w, 0], [0, rect.h], [0, -2 * rect.h]]) {
    await page.drag(mid, { x: mid.x + dx * 0.4, y: mid.y + dy * 0.4 }, 6);
    await page.frames(3);
  }
  await page.frames(10);

  const after = await page.eval(READOUT);
  assert.notEqual(after, before, "the gestures must have actually moved the view, or this proves nothing");
  t.diagnostic(`level readout: "${before}" -> "${after}"`);

  const tiles = page.requests.slice(firstIdx).filter((r) => r.url.includes("/api/tiles"));
  assert.ok(tiles.length > 50, `the zoom-out must have produced tile requests (got ${tiles.length})`);

  // (1) T-480's subject: every address is a node of the declared lattice.
  const offLattice = [];
  for (const r of tiles) {
    const a = addrOf(r.url);
    const fTile = axes.frequency.cell_hz * 2 ** a.levelF * a.cells;
    const tTile = axes.time.cell_s * 1e9 * 2 ** a.levelT * a.cells;
    const bad =
      !(Number.isSafeInteger(a.levelF) && a.levelF >= 0 && a.levelF <= capF) ||
      !(Number.isSafeInteger(a.levelT) && a.levelT >= 0 && a.levelT <= capT) ||
      !(Number.isSafeInteger(a.fIndex) && a.fIndex >= 0 && (a.fIndex + 1) * fTile <= ADDRESSABLE_HZ) ||
      !(Number.isSafeInteger(a.tIndex) && a.tIndex >= 0 && (a.tIndex + 1) * tTile <= ADDRESSABLE_NS);
    if (bad) offLattice.push(`${JSON.stringify(a)} <- ${r.url}`);
  }
  assert.deepEqual(offLattice.slice(0, 5), [],
    `${offLattice.length}/${tiles.length} requests named a node outside the declared lattice`);

  // (2) The residual, reported with the server's own words: an on-lattice node the route refuses.
  const refused = tiles.filter((r) => r.status >= 400 && r.status < 500);
  if (refused.length) {
    const first = refused[0];
    const body = await (await fetch(first.url, { headers: { Authorization: `Bearer ${TOKEN}` } })).json().catch(() => ({}));
    t.diagnostic(`${refused.length}/${tiles.length} on-lattice requests were refused ${first.status}: ${body.error ?? "(no body)"}`);
    t.diagnostic(`first refused address: ${JSON.stringify(addrOf(first.url))}`);
  } else {
    t.diagnostic(`0/${tiles.length} tile requests were refused`);
  }
});
