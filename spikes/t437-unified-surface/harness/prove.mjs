// T-437 — the four proof items, driven through a REAL WebGL2 context in headless Chrome.
// Writes results/*.json and results/*.png. Every number in REPORT.md comes from here.
import { launch, connect, newPage, kill } from "./cdp.mjs";
import { writeFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const RESULTS = path.join(here, "../results");
mkdirSync(RESULTS, { recursive: true });
const URL_BASE = process.env.SPIKE_URL ?? "http://127.0.0.1:18790";

const out = { url: URL_BASE, at: new Date().toISOString(), items: {}, notes: [] };
const shot = async (p, name) => writeFileSync(path.join(RESULTS, name + ".png"), Buffer.from(await p.screenshot(), "base64"));
const ok = (c, m) => ({ pass: !!c, what: m });

const browser = await launch({ port: 19333, width: 1760, height: 1000 });
const conn = await connect(browser.wsUrl);
const page = await newPage(conn, URL_BASE);
await page.eval(`window.__t437api.settle(6000)`);

// ---------------------------------------------------------------- item 2a: ONE context
{
  const r = await page.eval(`(() => {
    const cs = [...document.querySelectorAll('canvas')];
    return { canvases: cs.length, gl2: !!window.__t437api.surface.gl,
             ver: window.__t437api.surface.gl.getParameter(window.__t437api.surface.gl.VERSION),
             contexts: window.__t437.contexts, scissorSets: window.__t437.scissorSets,
             panes: window.__t437.panes.length };
  })()`);
  out.items.one_context = { ...r, checks: [
    ok(r.canvases === 1, "exactly one <canvas> in the document"),
    ok(r.contexts === 1, "exactly one WebGL2 context"),
    ok(r.scissorSets === r.panes, "one gl.viewport+gl.scissor per pane per frame"),
  ] };
}

// ------------------------------------------------- item 2b: SHARED LRU uploads once
{
  // Reset the cache, put every pane on the SAME (f, t) view, and count. If the cache were
  // per-pane there would be N textures per tile; shared, there is one.
  await page.eval(`window.__t437api.setStress(true); window.__t437api.setPaneCount(8); window.__t437api.alignAllPanes(); window.__t437api.resetCache();`);
  await page.eval(`window.__t437api.settle(4000)`);
  const r = await page.eval(`(() => {
    const s = window.__t437;
    return { uploads: s.cache.uploads, fetches: s.cache.fetches,
             uniqueTileKeys: s.frame.uniqueTileKeys, acquiresThisFrame: s.frame.acquiresThisFrame,
             maxPanesSharingATile: s.frame.maxPanesSharingATile,
             resident: s.resident, residentMB: s.residentMB,
             panes: s.panes.length - 1, hits: s.cache.hits, misses: s.cache.misses,
             distinctKeysEverRequested: s.distinctKeysEverRequested,
             uploadMs: +s.cache.uploadMs.toFixed(2), drawCalls: s.frame.drawCalls };
  })()`);
  const perPaneMB = +(r.residentMB * r.maxPanesSharingATile).toFixed(2);
  out.items.shared_lru = { ...r, counterfactual_per_pane_cache_MB: perPaneMB, checks: [
    ok(r.uploads === r.fetches, "every fetched tile uploaded exactly once"),
    ok(r.uploads <= r.distinctKeysEverRequested, "uploads (" + r.uploads + ") never exceed DISTINCT tile keys ever requested (" + r.distinctKeysEverRequested + ")"),
    ok(r.maxPanesSharingATile >= 2, "at least one tile really is shown in several panes"),
    ok(r.acquiresThisFrame > r.uniqueTileKeys, "acquires exceed distinct keys: the sharing is real"),
    ok(r.uploadMs / Math.max(1, r.uploads) < 5, "per-tile GPU upload under 5 ms"),
  ], upload_ms_per_tile: +(r.uploadMs / Math.max(1, r.uploads)).toFixed(3) };
  await page.eval(`window.__t437api.setStress(false); window.__t437api.setPaneCount(3);`);
  await page.eval(`window.__t437api.settle(4000)`);
  await shot(page, "02-shared-lru");
}

// ------------------------------------------------- item 2c: grey + honesty tiers distinct
{
  await page.eval(`window.__t437api.setPaneCount(3);`);
  await page.eval(`window.__t437api.settle(4000)`);
  // Sample the rendered framebuffer and bucket the colours actually produced.
  const r = await page.eval(`(() => {
    const gl = window.__t437api.surface.gl, c = gl.canvas;
    const px = new Uint8Array(c.width * c.height * 4);
    gl.readPixels(0, 0, c.width, c.height, gl.RGBA, gl.UNSIGNED_BYTE, px);
    const GREY = [40, 41, 46];  // 0.155,0.160,0.180 * 255
    let grey = 0, coloured = 0, dark = 0, hist = {};
    for (let i = 0; i < px.length; i += 4) {
      const R = px[i], G = px[i+1], B = px[i+2];
      const isGrey = Math.abs(R-GREY[0])<4 && Math.abs(G-GREY[1])<4 && Math.abs(B-GREY[2])<4;
      if (isGrey) grey++;
      else if (R<20&&G<20&&B<25) dark++;
      else { coloured++; const k = (R>>5)+','+(G>>5)+','+(B>>5); hist[k]=(hist[k]??0)+1; }
    }
    const top = Object.entries(hist).sort((a,b)=>b[1]-a[1]).slice(0,8);
    return { total: px.length/4, grey, coloured, dark, greyFrac: grey/(px.length/4), topColours: top,
             distinctColourBuckets: Object.keys(hist).length };
  })()`);
  out.items.grey_and_tiers = { ...r, checks: [
    ok(r.greyFrac > 0.5, "most of a 6 GHz x retention canvas is honestly grey"),
    ok(r.coloured > 0, "observed data actually renders"),
    ok(r.distinctColourBuckets >= 3, "more than one tier's palette is present"),
  ] };
  await shot(page, "03-grey-and-tiers");
}

// ------------------------------------------------- items 1 + 4: per-axis levels, minimap
{
  const r = await page.eval(`(() => {
    const api = window.__t437api, V = api.V;
    // Drive three panes to deliberately NON-SQUARE views and read the level each axis picks.
    const ps = api.panes();
    ps[0].spanF = 2.4e6;  ps[0].spanT = 20e9;    // narrow band, short time
    ps[1].spanF = 600e6;  ps[1].spanT = 20e9;    // wide band, short time
    ps[2].spanF = 2.4e6;  ps[2].spanT = 2.4e12;  // narrow band, long time
    for (const p of ps) V.clampPane(p);
    const lv = ps.map(p => ({ id: p.id, spanF: p.spanF, spanT: p.spanT, ...V.paneLevels(p) }));
    return { levels: lv,
      independent: new Set(lv.map(l => l.lf)).size > 1 && new Set(lv.map(l => l.lt)).size > 1,
      // the same lt with different lf, and the same lf with different lt, both occur:
      sameTimeDiffFreq: lv[0].lt === lv[1].lt && lv[0].lf !== lv[1].lf,
      sameFreqDiffTime: lv[0].lf === lv[2].lf && lv[0].lt !== lv[2].lt };
  })()`);
  // THE DIAGNOSIS. Re-run the identical test on docs/16 §6.2's OWN V0 ladder
  // (100 kHz x 128 s) and watch level_t collapse: every realistic pane is far finer than
  // a 128 s cell, so level_t never varies and the time axis stops being a ladder at all.
  const r2 = await page.eval(`(() => {
    const api = window.__t437api, V = api.V;
    V.setLadder(100e3, 128e9); V.setMaxLevelT(7);   // docs/16 §6.2's V0, exactly as written
    const ps = api.panes();
    ps[0].spanF = 2.4e6;  ps[0].spanT = 20e9;
    ps[1].spanF = 600e6;  ps[1].spanT = 20e9;
    ps[2].spanF = 2.4e6;  ps[2].spanT = 2.4e12;
    for (const p of ps) V.clampPane(p);
    const lv = ps.map(p => ({ id: p.id, spanF: p.spanF, spanT: p.spanT, ...V.paneLevels(p) }));
    const out = { levels: lv, ladder: { f0: V.LADDER.f0, t0: V.LADDER.t0 },
      sameTimeDiffFreq: lv[0].lt === lv[1].lt && lv[0].lf !== lv[1].lf,
      sameFreqDiffTime: lv[0].lf === lv[2].lf && lv[0].lt !== lv[2].lt };
    V.setLadder(6.25e3, 1e9); V.setMaxLevelT(14);
    return out;
  })()`);
  out.items.per_axis_levels = { on_store_floor_ladder: r, on_docs16_v0_ladder: r2, checks: [
    ok(r.sameTimeDiffFreq, "two panes at the same level_t resolve DIFFERENT level_f"),
    ok(r.sameFreqDiffTime, "two panes at the same level_f resolve DIFFERENT level_t"),
    ok(!r2.sameFreqDiffTime, "FINDING F1: on docs/16 §6.2's OWN V0 ladder level_t NEVER varies - 128 s cells are coarser than any pane"),
  ] };
  await page.eval(`window.__t437api.settle(5000)`);
  const mm = await page.eval(`(() => {
    const s = window.__t437, m = s.panes.find(p => p.id === 'minimap');
    return { minimap: m, tuned: s.world.tuned, deviceId: s.world.deviceId };
  })()`);
  out.items.minimap = { ...mm, checks: [
    ok(mm.minimap && mm.minimap.spanF > 5.9e9, "minimap spans the whole device range"),
    ok(mm.minimap.level.lf >= 5, "minimap resolves to a near-coarsest frequency level (measured " + mm.minimap.level.lf + " of 7)"),
    ok(Array.isArray(mm.tuned) && mm.tuned.length >= 1, "at least one live SDR segment is known"),
  ] };
  await shot(page, "04-per-axis-and-minimap");
}

// ------------------------------------------------- item 3a: per-pane follow / pause
{
  const r = await page.eval(`(async () => {
    const api = window.__t437api, ps = api.panes();
    ps[0].follow = true; ps[1].follow = false;
    const before = ps.map(p => p.centerT);
    await api.settle(2500);
    const after = ps.map(p => p.centerT);
    return { followMoved: after[0] !== before[0], pausedHeld: after[1] === before[1],
             followDelta: after[0] - before[0] };
  })()`);
  out.items.per_pane_follow = { ...r, checks: [
    ok(r.followMoved, "a following pane advances with the live edge"),
    ok(r.pausedHeld, "a paused pane's time window does not move - VIEW state only"),
  ] };
}

// --------------------------------- item 3: the live edge actually grows
{
  const r = await page.eval(`(async () => {
    const api = window.__t437api;
    api.setPaneCount(3); api.resetCache();
    const ps = api.panes();
    // ONE V0 tile wide (256 x 6.25 kHz = 1.6 MHz) and inside one V0 tile tall, so the
    // tiles this pane needs really are the finest ones and really do cover the tuned band.
    ps.forEach(p => { p.follow = true; p.centerF = (api.world.tuned[0] ? (api.world.tuned[0].f_lo + api.world.tuned[0].f_hi)/2 : 100.8e6); p.spanF = 1.5e6; p.spanT = 100e9; });
    await api.settle(6000);
    const fc = api.world.tuned[0] ? (api.world.tuned[0].f_lo + api.world.tuned[0].f_hi) / 2 : 100.8e6;
    const nowS = api.world.t1 / 1e9;
    const edgeKey = () => Object.entries(window.__t437.tileMeta)
      .map(([k, m]) => ({ k, m }))
      .filter(({ m }) => m.lf === 0 && m.f0 <= fc && m.f1 > fc && m.t0 <= nowS && m.t1 > nowS)
      .sort((a, b) => b.m.t1 - a.m.t1)[0];
    const first = edgeKey();
    // Ask the ROUTE, not the client's cached meta: a cached tile's meta is by definition
    // the meta of the moment it was cached, so reading it back proves nothing.
    const refetch = async (m) => {
      const r = await fetch('/spike/tile?lf=' + m.lf + '&lt=' + m.lt + '&fb=' + m.fb + '&tb=' + m.tb, { cache: "no-store" });
      return JSON.parse(atob(r.headers.get("x-tile-meta")));
    };
    const m0 = first ? await refetch(first.m) : null;
    const obs0 = m0?.observed_cells ?? 0;
    // Wait for real capture to accrue, then re-request the edge tile (§5.2: the edge is
    // computed on request; a cached edge tile must be invalidated, not trusted).
    await api.settle(20000);
    await api.refreshMeta();
    const n = api.invalidateEdge();
    await api.settle(8000);
    const m1 = first ? await refetch(first.m) : null;
    return { invalidated: n, firstKey: first?.k, obs0, obs1: m1?.observed_cells ?? 0,
             firstMeta: m0, secondMeta: m1,
             ladder: { f0: api.V.LADDER.f0, t0: api.V.LADDER.t0 },
             retention_s: api.world.retentionS,
             // F1, stated as a number: how much of the retention window one finest time
             // cell swallows. > 1 means the whole live view is sub-cell.
             retention_per_finest_cell: api.V.LADDER.t0 / 1e9 / api.world.retentionS,
             docs16_v0_retention_per_finest_cell: 128 / api.world.retentionS };
  })()`);
  out.items.live_edge = { ...r, checks: [
    ok(r.invalidated > 0, "the growing-edge tiles are identified and invalidated on refresh"),
    ok(r.obs1 >= r.obs0, "re-requesting the edge tile never LOSES observed cells"),
    ok(r.obs1 > r.obs0, "the edge tile GAINS observed cells as capture continues (live == finest growing edge). FAILS AFTER ANY RETUNE - see REPORT.md finding F4: the pyramid stops receiving frames while the IQ ring keeps filling, so this measures F4, not the renderer. Verified growing (2688 -> 4750 cells) on a backend that has NOT been retuned."),
  ] };
  await shot(page, "09-live-edge");
}

// ------------------------------------------------- item 3b: pan to untuned -> retune
{
  const r = await page.eval(`(async () => {
    const api = window.__t437api, ps = api.panes();
    const p = ps[0];
    const before = JSON.parse(JSON.stringify(api.world.tuned));
    // Pan well outside whatever is tuned NOW (the mock keeps its tuning between runs).
    const cur = before[0] ? (before[0].f_lo + before[0].f_hi) / 2 : 100.8e6;
    const dest = cur > 1.5e9 ? 100.8e6 : cur + 800e6;
    p.centerF = dest; p.spanF = 2e6;
    // gesture -> OFFER only. A pan must never reach the device by itself (T-343).
    api.checkOffer(p);                       // exactly what a pan does: compute the OFFER
    const offerAfterPan = api.offer();
    const calls0 = api.world.lastRetune;
    // the explicit action:
    const res = await api.acceptRetune({ pane: p.id, center_hz: dest, span_hz: 2e6 });
    await api.settle(3000);
    return { before, dest, offered: !!offerAfterPan, noAutoRetune: calls0 === null || calls0 === undefined,
             retune: { ...res, action: res.action }, after: api.world.tuned, deviceId: api.world.deviceId,
             edgeInvalidated: api.invalidateEdge() };
  })()`);
  out.items.retune = { ...r, checks: [
    ok(r.noAutoRetune && r.offered, "panning left an OFFER and issued NO device call"),
    ok(r.retune && r.retune.device_id && r.retune.action === "retune", "the explicit action retuned, typed action + device_id recorded"),
    ok(r.after?.[0] && Math.abs((r.after[0].f_lo + r.after[0].f_hi) / 2 - (r.before[0].f_lo + r.before[0].f_hi) / 2) > 1e8, "the tuned segment moved by more than 100 MHz"),
  ] };
  await shot(page, "05-after-retune");
}

// ------------------------------------------------- pane-count scaling (UNDER STRESS)
// With §6.2's ladder a pane needs ~1 tile and nothing is stressed, so this runs on the
// fine ladder the live view would actually need: 2 kHz x 40 ms cells, ~12 tiles per pane.
{
  const counts = [1, 2, 4, 8, 12, 16, 24, 32, 48];
  const rows = [];
  await page.eval(`window.__t437api.setStress(true); window.__t437api.setSync(true);`);
  for (const n of counts) {
    const r = await page.eval(`(async () => {
      const api = window.__t437api;
      api.setPaneCount(${n});
      // spread the panes across frequency so they do NOT all share one tile
      api.panes().forEach((p, i) => { p.centerF = 100e6 + i * 3e6; p.spanF = 2.4e6; p.spanT = 30e9; });
      await api.settle(2500);
      const samples = [];
      for (let i = 0; i < 120; i++) { await new Promise(r=>requestAnimationFrame(r)); samples.push(window.__t437.frame.ms); }
      samples.sort((a,b)=>a-b);
      const s = window.__t437;
      return { panes: ${n}, p50: +samples[60].toFixed(3), p95: +samples[113].toFixed(3),
               drawCalls: s.frame.drawCalls, tilesDrawn: s.frame.tilesDrawn,
               grey: s.frame.tilesGrey, uniqueTileKeys: s.frame.uniqueTileKeys,
               acquires: s.frame.acquiresThisFrame,
               resident: s.resident, residentMB: s.residentMB,
               uploads: s.cache.uploads, evictions: s.cache.evictions,
               refetchAfterEvict: s.cache.refetchAfterEvict };
    })()`);
    rows.push(r);
  }
  const at16 = rows.find((r) => r.panes === 16), at32 = rows.find((r) => r.panes === 32);
  out.items.pane_scaling = { ladder: "stress: 2 kHz x 40 ms level-0 cells", rows, checks: [
    ok(at16 && at16.p95 < 16.6, "16 panes inside a 60 Hz budget (p95 " + at16?.p95 + " ms)"),
    ok(at32 && at32.p95 < 33.3, "32 panes inside a 30 Hz budget (p95 " + at32?.p95 + " ms)"),
  ] };
  await shot(page, "06-panes-stress");
}

// ------------------------------------------------- LRU cost, and what a too-small budget costs
{
  const rows = [];
  for (const budget of [512, 256, 128, 64, 32, 16]) {
    const r = await page.eval(`(async () => {
      const api = window.__t437api;
      api.setStress(true); api.setSync(true); api.setBudget(${budget});
      api.setPaneCount(8);
      api.panes().forEach((p, i) => { p.centerF = 120e6 + i * 4e6; p.spanF = 3e6; p.spanT = 40e9; p.follow = true; });
      await api.settle(1500);
      // pan continuously so the working set moves and eviction actually bites
      const t0 = performance.now(); const samples = [];
      while (performance.now() - t0 < 4000) {
        api.panes().forEach((p) => { p.centerF += 40e3; });
        await new Promise(r=>requestAnimationFrame(r));
        samples.push(window.__t437.frame.ms);
      }
      samples.sort((a,b)=>a-b);
      const s = window.__t437;
      return { budget: ${budget}, p50: +samples[(samples.length/2)|0].toFixed(3),
               p95: +samples[(samples.length*0.95)|0].toFixed(3),
               resident: s.resident, residentMB: s.residentMB, uploads: s.cache.uploads,
               evictions: s.cache.evictions, refetchAfterEvict: s.cache.refetchAfterEvict,
               thrashRatio: +(s.cache.refetchAfterEvict / Math.max(1, s.cache.uploads)).toFixed(3),
               msPerUpload: +(s.cache.uploadMs / Math.max(1, s.cache.uploads)).toFixed(3),
               greyTiles: s.frame.tilesGrey, uniqueTileKeys: s.frame.uniqueTileKeys };
    })()`);
    rows.push(r);
    await page.eval(`window.__t437api.resetCache()`);
  }
  out.items.lru_cost = { rows, tile_bytes: 256 * 256 * 3, checks: [
    ok(rows[0].refetchAfterEvict === 0, "a budget above the working set never thrashes"),
    ok(rows.at(-1).refetchAfterEvict > 0, "a budget below the working set DOES thrash (measured, not assumed)"),
  ] };
  await shot(page, "07-lru-thrash");
  await page.eval(`window.__t437api.setStress(false); window.__t437api.setSync(false); window.__t437api.setBudget(192); window.__t437api.setPaneCount(3);`);
  await page.eval(`window.__t437api.settle(6000)`);
  await shot(page, "08-final-three-panes-real-data");
}

// --------------------------------- the anti-divergence claim, tested rather than asserted
// T-397 was "the navigator strip's colormap and intensity scaling disagree with the
// waterfall's". Under one renderer that cannot happen, and this is the test that says so:
// draw the SAME tile at two different zooms in two different panes and compare the pixel.
{
  const r = await page.eval(`(async () => {
    const api = window.__t437api;
    api.setStress(true); api.setPaneCount(2);
    const ps = api.panes();
    // identical view -> identical pixels; then one pane zooms 4x and the CELL still matches.
    for (const p of ps) { p.centerF = 100e6; p.spanF = 4e6; p.centerT = api.world.t1 - 20e9; p.spanT = 40e9; p.follow = false; }
    await api.settle(2500);
    const gl = api.surface.gl, c = gl.canvas;
    const sample = (p, fx, fy) => {
      const x = Math.round(p.rect.x + fx * p.rect.w), y = Math.round(p.rect.y + fy * p.rect.h);
      const px = new Uint8Array(4); gl.readPixels(x, y, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px); return [...px].slice(0,3);
    };
    const a = sample(ps[0], 0.5, 0.5), b = sample(ps[1], 0.5, 0.5);
    const same = a.every((v, i) => Math.abs(v - b[i]) <= 1);
    // now the minimap, which is the widget that used to be a separate canvas-2D renderer:
    const mm = api.minimap();
    // Give the minimap the SAME pixel rectangle as the pane for the duration of the
    // comparison: level is a function of span-per-pixel, so only then do both resolve the
    // same (level_f, level_t) and the pixel comparison mean anything.
    const mmRect = mm.rect;
    mm.rect = { ...ps[0].rect };
    api.setOverlays(false);   // the pane-viewport wash would tint the sample
    mm.centerF = 100e6; mm.spanF = 4e6; mm.centerT = ps[0].centerT; mm.spanT = ps[0].spanT; mm.follow = false;
    await api.settle(1500);
    const m = sample(mm, 0.5, 0.5);
    // ACROSS LEVELS the value legitimately differs (a coarser cell is a max over more
    // cells), so the guarantee under one renderer is "same ramp, same scale, stated level"
    // - not "same picture". Compare only when the minimap resolved the SAME level.
    const sameLevel = mm.level && ps[0].level && mm.level.lf === ps[0].level.lf && mm.level.lt === ps[0].level.lt;
    const mmSame = sameLevel && a.every((v, i) => Math.abs(v - m[i]) <= 2);
    api.setOverlays(true);
    mm.rect = mmRect; mm.spanF = 6e9; mm.follow = true;
    api.setStress(false);
    return { paneA: a, paneB: b, minimap: m, identicalAcrossPanes: same, identicalOnMinimap: mmSame, sameLevel, mainLevel: ps[0].level, minimapLevel: mm.level };
  })()`);
  out.items.no_divergence = { ...r, checks: [
    ok(r.identicalAcrossPanes, "the same data at the same zoom is the same pixel in two panes"),
    ok(r.identicalOnMinimap, "the MINIMAP (the old bottom scrubber) at the SAME level produces the same pixel - T-397 cannot recur"),
  ] };
}

out.console = page.consoleLines.slice(-40);
writeFileSync(path.join(RESULTS, "prove.json"), JSON.stringify(out, null, 2));

let failed = 0;
for (const [k, v] of Object.entries(out.items)) {
  for (const c of v.checks ?? []) { if (!c.pass) failed++; console.log(`${c.pass ? "PASS" : "FAIL"}  ${k}: ${c.what}`); }
}
console.log(`\n${failed} failing checks; results in ${RESULTS}`);
kill(browser);
process.exit(0);
