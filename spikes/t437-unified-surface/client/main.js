// T-437 spike — the unified surface. ONE canvas, ONE WebGL2 context, N panes.
//
// What used to be four things is one thing here:
//   the live waterfall      -> a pane with follow = true at the finest level
//   the history view        -> a pane with follow = false, zoomed out in time
//   the left time navigator -> a pane, narrow, wide in time, at the main pane's f range
//   the bottom freq navigator / minimap -> a pane at a coarse level over the whole device range
// They are not four widgets that must be kept consistent. They are four VIEWPORTS, and the
// consistency is structural: same shader, same ramp, same cache, same addressing.

import { Surface, toClip, TIER } from "./render.js";
import { TileCache, TILE_BYTES } from "./tilecache.js";
import * as V from "./view.js";

const canvas = document.getElementById("surface");
const hud = document.getElementById("hud");

const surface = new Surface(canvas);
const gl = surface.gl;

// ---- tile source: the spike route (harness/serve.mjs). Grey on failure, never fabricate.
const tileMeta = new Map();
async function fetchTile(t) {
  const r = await fetch(`/spike/tile?lf=${t.lf}&lt=${t.lt}&fb=${t.fb}&tb=${t.tb}`, { cache: "no-store" });
  if (!r.ok) return null;
  const meta = JSON.parse(atob(r.headers.get("x-tile-meta")));
  tileMeta.set(`${t.lf}/${t.lt}/${t.fb}/${t.tb}`, meta);
  const buf = new Uint8Array(await r.arrayBuffer());
  if (buf.byteLength !== 256 * 256 * 3) return null;
  return { rgb: buf };
}

// A LOCAL STRESS SOURCE. It generates tiles in the client so the renderer/LRU cost can be
// measured with a realistic working set. It is NOT data and never claims to be: it is only
// enabled by an explicit ?stress=1 and its tiles are marked survey-overview. It exists
// because §6.2's ladder makes every pane need ~1 tile, so the real source cannot stress the
// thing the spike is supposed to cost out.
let stress = false;
function synthTile(t) {
  const rgb = new Uint8Array(256 * 256 * 3);
  const seed = ((t.lf * 131 + t.lt) * 8191 + t.fb * 7919 + t.tb * 104729) >>> 0;
  let s = seed || 1;
  const rnd = () => ((s = (s * 1664525 + 1013904223) >>> 0) / 4294967296);
  // a few synthetic carriers, and genuine holes so grey is still exercised
  const hole = rnd() < 0.35;
  for (let y = 0; y < 256; y++) for (let x = 0; x < 256; x++) {
    const i = (y * 256 + x) * 3;
    if (hole && y > 128) continue;            // leave tier 0 => grey
    const v = 40 + 90 * Math.exp(-((x - 60) ** 2) / 40) + 70 * Math.exp(-((x - 190) ** 2) / 12) + 12 * rnd();
    rgb[i] = Math.max(1, Math.min(255, v | 0));
    rgb[i + 1] = 255;
    rgb[i + 2] = 1;                           // survey-overview: synthetic is never "live"
  }
  return { rgb };
}

const cache = new TileCache(gl, (t) => (stress ? synthTile(t) : fetchTile(t)), {
  budgetTiles: +(new URLSearchParams(location.search).get("budget") ?? 192),
  inflightCap: 12,
});

// ---- world state -------------------------------------------------------------
const world = {
  t1: Date.now() / 1e3 * 1e9,  // live edge, ns
  retentionS: 120,
  tuned: [],                   // [{ device_id, f_lo, f_hi }] - lit segments on the minimap
  deviceId: null,
  navigation: null,
  lastRetune: null,
};

async function refreshMeta() {
  try {
    const m = await fetch("/spike/meta").then((r) => r.json());
    world.t1 = m.window.t1_s * 1e9;
    world.retentionS = m.window.retention_s ?? 120;
    world.deviceId = m.device?.id ?? null;
    world.navigation = m.navigation;
    const tn = m.tuning ?? {};
    const c = tn.center_hz, sr = tn.sample_rate_hz;
    world.tuned = (c && sr) ? [{ device_id: m.device?.id, f_lo: c - sr / 2, f_hi: c + sr / 2 }] : [];
    world.serverStats = m.stats;
  } catch {}
}

// ---- panes -------------------------------------------------------------------
// Deliberately laid out as the four surfaces this replaces, so the exit criterion is
// judged on the real thing and not on an abstract grid of squares.
const PANE_DEFS = [
  { id: "time-nav", w: 0.10, follow: true, spanF: 2.4e6, spanT: 120e9, label: "time nav (was the LEFT scrubber)" },
  { id: "live", w: 0.52, follow: true, spanF: 2.4e6, spanT: 40e9, label: "live (finest growing edge)" },
  { id: "history", w: 0.38, follow: false, spanF: 40e6, spanT: 900e9, label: "history (was the SEPARATE view)" },
];
const MINIMAP_H = 0.16;

let panes = [];
function layout() {
  const W = canvas.width, H = canvas.height;
  const mmH = Math.round(H * MINIMAP_H);
  const topH = H - mmH;
  let x = 0;
  panes = PANE_DEFS.map((d, i) => {
    const w = i === PANE_DEFS.length - 1 ? W - x : Math.round(W * d.w);
    const p = V.makePane(d.id, {
      centerF: 100.8e6, spanF: d.spanF, centerT: world.t1 - d.spanT / 2, spanT: d.spanT,
      follow: d.follow, rect: { x, y: mmH, w: w - 2, h: topH },
    });
    p.label = d.label;
    x += w;
    return p;
  });
  // The minimap is NOT a special widget: it is a pane over the whole device range at a
  // coarse level, plus two overlays. §8.3.
  minimap = V.makePane("minimap", {
    centerF: (V.F_LO_HZ + V.F_HI_HZ) / 2, spanF: V.F_HI_HZ - V.F_LO_HZ,
    centerT: world.t1 - (world.retentionS * 1e9) / 2, spanT: world.retentionS * 1e9,
    follow: true, rect: { x: 0, y: 0, w: W, h: mmH - 2 },
  });
  minimap.label = "minimap (was the BOTTOM scrubber)";
}
let minimap = null;

function resize() {
  const dpr = Math.min(2, window.devicePixelRatio || 1);
  canvas.width = Math.round(canvas.clientWidth * dpr);
  canvas.height = Math.round(canvas.clientHeight * dpr);
  layout();
}

// ---- draw --------------------------------------------------------------------
let frameStats = { tilesDrawn: 0, tilesGrey: 0, distinctTiles: 0, drawCalls: 0, ms: 0, panes: 0 };

const sharing = new Map(); // key -> how many panes acquired it THIS frame

function drawPane(p, opts = {}) {
  const { lf, lt } = V.paneLevels(p);
  p.level = { lf, lt };
  if (p.follow) p.centerT = world.t1 - p.spanT / 2;
  const box = V.paneBox(p);
  surface.bindPane(p.rect);
  const tiles = V.tilesFor(box, lf, lt);
  const pxPerCellF = p.rect.w / (p.spanF / V.fCellHz(lf));
  const pxPerCellT = p.rect.h / (p.spanT / V.tCellNs(lt));
  for (const t of tiles) {
    const k = `${t.lf}/${t.lt}/${t.fb}/${t.tb}`;
    sharing.set(k, (sharing.get(k) ?? 0) + 1);
    const e = cache.acquire(t);
    if (!e) { frameStats.tilesGrey++; continue; }   // grey: already cleared to it
    // Clip the tile's uv to the visible part so a tile larger than the pane still lands right.
    const clip = toClip({ f0: t.f0, f1: t.f1, t0: t.t0, t1: t.t1 }, box);
    surface.drawTile(e.tex, clip, [0, 0], [1, 1], [pxPerCellF, pxPerCellT], 1);
    frameStats.tilesDrawn++;
    opts.seen?.add(`${t.lf}/${t.lt}/${t.fb}/${t.tb}`);
  }
  return box;
}

let drawOverlays = true;
function drawMinimap() {
  const box = drawPane(minimap, {});
  if (!drawOverlays) return;
  // pane-viewport rectangles (§8.3 / item 4)
  for (const p of panes) {
    const b = V.paneBox(p);
    const r = toClip(b, box);
    const cl = [Math.max(-1, r[0]), Math.max(-1, r[1]), Math.min(1, r[2]), Math.min(1, r[3])];
    if (cl[2] - cl[0] < 0.004) { cl[2] = cl[0] + 0.004; }      // a 2.4 MHz pane over 6 GHz is sub-pixel
    surface.drawFlat(cl, [0.35, 0.75, 1.0, 0.16]);
    surface.strokeRect(cl, [0.45, 0.85, 1.0, 0.95], 1.5, minimap.rect);
  }
  // per-SDR live segments: where hardware is ACTUALLY tuned right now.
  for (const s of world.tuned) {
    const x0 = 2 * ((s.f_lo - box.f0) / (box.f1 - box.f0)) - 1;
    const x1 = 2 * ((s.f_hi - box.f0) / (box.f1 - box.f0)) - 1;
    surface.drawFlat([Math.max(-1, x0), -1, Math.min(1, Math.max(x1, x0 + 0.004)), -0.90], [0.20, 1.0, 0.55, 0.95]);
  }
}

let syncEveryFrame = false;
function render() {
  const t0 = performance.now();
  const seen = new Set();
  sharing.clear();
  frameStats = { tilesDrawn: 0, tilesGrey: 0, distinctTiles: 0, drawCalls: 0, ms: 0, panes: panes.length + 1 };
  surface.begin();
  for (const p of panes) drawPane(p, { seen });
  drawMinimap();
  surface.end();
  frameStats.distinctTiles = seen.size;
  frameStats.drawCalls = surface.drawCalls;
  // Chrome's gl.finish() is non-blocking; a 1-px readPixels forces the sync, so `ms`
  // includes the GPU frame cost rather than just the JS that queued it. (Spike S3's
  // finding, reused: without this every measurement reads as ~0 ms.)
  if (syncEveryFrame) { const px = new Uint8Array(4); gl.readPixels(0, 0, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px); }
  frameStats.ms = performance.now() - t0;
  let maxShare = 0, uniq = 0;
  for (const n of sharing.values()) { uniq++; if (n > maxShare) maxShare = n; }
  frameStats.uniqueTileKeys = uniq;
  frameStats.maxPanesSharingATile = maxShare;
  frameStats.acquiresThisFrame = [...sharing.values()].reduce((a, b) => a + b, 0);
  window.__t437 = {
    frame: frameStats, cache: cache.stats, distinctKeysEverRequested: cache.everRequested.size,
    resident: cache.residentTiles, residentMB: +(cache.residentBytes / 1e6).toFixed(2),
    contexts: surface.contexts, scissorSets: surface.scissorSets,
    panes: panes.concat([minimap]).map((p) => ({
      id: p.id, level: p.level, follow: p.follow,
      spanF: p.spanF, spanT: p.spanT, centerF: p.centerF, centerT: p.centerT,
    })),
    world, tileMeta: Object.fromEntries(tileMeta),
    lastRetune: world.lastRetune,
  };
  if (hud) hud.textContent = hudText();
  requestAnimationFrame(render);
}

function fmtHz(h) { return h >= 1e9 ? (h / 1e9).toFixed(3) + " GHz" : h >= 1e6 ? (h / 1e6).toFixed(3) + " MHz" : (h / 1e3).toFixed(1) + " kHz"; }
function hudText() {
  const c = cache.stats;
  const lines = [
    `ONE WebGL2 context | panes ${panes.length} + minimap | scissor sets/frame ${surface.scissorSets}`,
    `frame ${frameStats.ms.toFixed(2)} ms  draws ${frameStats.drawCalls}  tiles drawn ${frameStats.tilesDrawn}  GREY (no data) ${frameStats.tilesGrey}`,
    `LRU: uploads ${c.uploads} = distinct tiles (a tile in 2 panes uploads once) | hits ${c.hits} misses ${c.misses} evict ${c.evictions} refetch-after-evict ${c.refetchAfterEvict}`,
    `resident ${cache.residentTiles} tiles = ${(cache.residentBytes / 1e6).toFixed(1)} MB (${(TILE_BYTES / 1024) | 0} KB/tile)`,
  ];
  for (const p of panes.concat([minimap]))
    lines.push(`  ${p.id.padEnd(9)} lf=${p.level?.lf} lt=${p.level?.lt}  ${fmtHz(p.spanF)} x ${(p.spanT / 1e9).toFixed(0)} s  ${p.follow ? "FOLLOW" : "paused (view only)"}  ${p.label ?? ""}`);
  if (world.lastRetune) lines.push(`  retune: ${JSON.stringify(world.lastRetune)}`);
  return lines.join("\n");
}

// ---- gestures ----------------------------------------------------------------
// Both axes wheel-zoom; neither reaches the device. Panning in frequency past the tuned
// range leaves a RETUNE OFFER (T-343: an explicit action, never the continuation of a pan).
let retuneOffer = null;
function paneAt(px, py) {
  const y = canvas.height - py;
  for (const p of panes.concat([minimap])) {
    const r = p.rect;
    if (px >= r.x && px < r.x + r.w && y >= r.y && y < r.y + r.h) return p;
  }
  return null;
}
canvas.addEventListener("wheel", (e) => {
  e.preventDefault();
  const dpr = canvas.width / canvas.clientWidth;
  const p = paneAt(e.offsetX * dpr, e.offsetY * dpr);
  if (!p) return;
  const k = Math.exp(e.deltaY * 0.0015);
  if (e.shiftKey) { p.spanF = Math.max(1e5, Math.min(V.F_HI_HZ - V.F_LO_HZ, p.spanF * k)); V.clampPane(p); }
  else { p.spanT = Math.max(2e9, Math.min(world.retentionS * 1e9 * 64, p.spanT * k)); }
  checkOffer(p);
}, { passive: false });

let drag = null;
canvas.addEventListener("pointerdown", (e) => {
  const dpr = canvas.width / canvas.clientWidth;
  const p = paneAt(e.offsetX * dpr, e.offsetY * dpr);
  if (p) drag = { p, x: e.clientX, y: e.clientY };
});
window.addEventListener("pointerup", () => { drag = null; });
window.addEventListener("pointermove", (e) => {
  if (!drag) return;
  const { p } = drag;
  p.centerF -= ((e.clientX - drag.x) / p.rect.w) * p.spanF;
  if (e.clientY !== drag.y) { p.follow = false; p.centerT += ((e.clientY - drag.y) / p.rect.h) * p.spanT; }
  drag.x = e.clientX; drag.y = e.clientY;
  V.clampPane(p);
  checkOffer(p);
});

function checkOffer(p) {
  const b = V.paneBox(p);
  const covered = world.tuned.some((s) => b.f0 >= s.f_lo && b.f1 <= s.f_hi);
  retuneOffer = covered ? null : { pane: p.id, center_hz: p.centerF, span_hz: p.spanF };
  const el = document.getElementById("offer");
  if (el) {
    el.hidden = !retuneOffer || p.id === "minimap";
    if (retuneOffer) el.textContent = `retune to ${fmtHz(p.centerF)} (${fmtHz(p.spanF)})? — this is a DEVICE action`;
  }
}

document.getElementById("offer")?.addEventListener("click", () => acceptRetune());

/** The one path to the device, and only from an explicit click. */
async function acceptRetune(offer = retuneOffer) {
  if (!offer) return null;
  // Snap to an achievable configuration first (/api/navigation, via /spike/meta).
  const nav = world.navigation;
  let rate = null;
  if (nav?.spans_hz?.length) {
    const fit = nav.spans_hz.filter((s) => s >= offer.span_hz).sort((a, b) => a - b)[0];
    rate = fit ?? null;
  }
  const r = await fetch("/spike/retune", {
    method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ center_hz: offer.center_hz, sample_rate_hz: rate }),
  }).then((x) => x.json());
  world.lastRetune = { requested: offer, device_id: r.device_id, action: r.action, calls: r.calls?.map((c) => `${c.path}:${c.status}`) };
  await refreshMeta();
  // The live edge moved: drop the finest tiles that covered the OLD tuning so they are
  // rewritten rather than served stale. §5.2's "computed on request at the live edge".
  invalidateEdge();
  return world.lastRetune;
}

/** Live edge: the finest tiles for tuned ranges are the ones still being written. */
function invalidateEdge() {
  let n = 0;
  for (const key of [...cache.map.keys()]) {
    const [lf, lt, fb, tb] = key.split("/").map(Number);
    const t0 = tb * V.tBlockNs(lt), t1 = t0 + V.tBlockNs(lt);
    if (t1 >= world.t1 - V.tCellNs(lt)) { cache.invalidate(key); n++; }
  }
  return n;
}

// ---- test hooks (the CDP harness drives these) -------------------------------
window.__t437api = {
  panes: () => panes, minimap: () => minimap, cache, surface, world, V,
  setPaneCount: (n) => {
    PANE_DEFS.length = 0;
    for (let i = 0; i < n; i++) PANE_DEFS.push({
      id: "p" + i, w: 1 / n, follow: i % 2 === 0,
      spanF: 2.4e6 * (1 + i), spanT: 40e9 * (1 + i), label: "synthetic pane " + i,
    });
    layout();
  },
  /** Force every pane onto the SAME view so the shared-LRU claim is testable. */
  alignAllPanes: () => {
    for (const p of panes) { p.centerF = 100.8e6; p.spanF = 2.4e6; p.spanT = 40e9; p.follow = true; p.centerT = world.t1 - 20e9; }
  },
  invalidateEdge, acceptRetune, refreshMeta, checkOffer,
  setStress: (on, fHz = 2e3, tNs = 40e6) => {
    stress = !!on;
    if (on) { V.setLadder(fHz, tNs); V.setMaxLevelT(14); } else { V.setLadder(6.25e3, 1e9); V.setMaxLevelT(14); }
    cache.dispose(); cache.stats.uploads = 0; cache.stats.hits = 0; cache.stats.misses = 0;
    cache.stats.evictions = 0; cache.stats.refetchAfterEvict = 0; cache.stats.fetches = 0;
    cache.stats.requests = 0; cache.stats.uploadMs = 0; cache.evicted.clear(); cache.everRequested.clear();
    layout();
  },
  setSync: (on) => { syncEveryFrame = !!on; },
  setOverlays: (on) => { drawOverlays = !!on; },
  resetCache: () => { cache.dispose(); for (const k of Object.keys(cache.stats)) cache.stats[k] = 0; cache.evicted.clear(); cache.everRequested.clear(); },
  setBudget: (n) => { cache.budgetTiles = n; cache.evict(); },
  offer: () => retuneOffer,
  readPixel: (x, y) => {
    const px = new Uint8Array(4);
    gl.readPixels(x, canvas.height - y, 1, 1, gl.RGBA, gl.UNSIGNED_BYTE, px);
    return [...px];
  },
  settle: async (ms = 2500) => { const t = Date.now(); while (Date.now() - t < ms) await new Promise((r) => setTimeout(r, 50)); },
};

window.addEventListener("resize", resize);
await refreshMeta();
resize();
setInterval(refreshMeta, 1000);
requestAnimationFrame(render);
