// T-437 spike server.
//
// TWO JOBS:
//  (a) serve client/ statically;
//  (b) STUB THE TILE ROUTE. This is explicitly a STUB and it is written in Node on purpose,
//      so nobody mistakes it for the real /api/tile. T-434 owns the backend de-welding
//      (LevelConfig::t_factor); docs/16 §7 step 5 owns the real route. What this proves is
//      that the ADDRESSING works end to end — a client asking for (level_f, level_t,
//      f_block, t_block) with the two levels INDEPENDENT, answered from the real backend
//      running over the real mock SDR device.
//
// It never talks to a device except through the backend's own gated routes, and it refuses
// to start against a backend on 8789/8899/8900 (the user's live demo).

import { createServer } from "node:http";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const CLIENT = path.join(here, "../client");

const argv = process.argv.slice(2);
const arg = (k, d) => { const i = argv.indexOf("--" + k); return i >= 0 ? argv[i + 1] : d; };
const PORT = +arg("port", 18790);
const BACKEND = arg("backend", "http://127.0.0.1:18787");
const TOKEN = arg("token", process.env.HK_TOKEN ?? "t437spiketoken0123");

for (const forbidden of ["8789", "8899", "8900"]) {
  if (BACKEND.endsWith(":" + forbidden)) throw new Error(`refusing: ${forbidden} is the live demo's`);
  if (PORT === +forbidden) throw new Error(`refusing to bind ${forbidden}`);
}

// ---- the view scheme (mirrors client/view.js; §6.2) ----
const CELLS = 256, F_CELL0 = 6.25e3, T_CELL0_S = 1;  // the store's real level-0 floor; see REPORT.md F1
const fCell = (lf) => F_CELL0 * 2 ** lf;
const tCellS = (lt) => T_CELL0_S * 2 ** lt;
const fBlock = (lf) => fCell(lf) * CELLS;
const tBlockS = (lt) => tCellS(lt) * CELLS;

const api = async (p) => {
  const r = await fetch(BACKEND + p, { headers: { Authorization: "Bearer " + TOKEN } });
  if (!r.ok) throw new Error(`${p} -> ${r.status} ${(await r.text()).slice(0, 200)}`);
  return r.json();
};

const stats = { tiles: 0, historyCalls: 0, coverageCalls: 0, msTotal: 0, byTier: {} };

/**
 * Fold a row-major (nt x nf) plane onto CELLS x CELLS.
 * MAX over every source cell in the output cell when reducing (§4: folding never lowers a
 * value), nearest-neighbour REPLICATION when the source is coarser than the tile — and
 * replication is reported, never silently passed off as detail (T-342 / T-411).
 */
function resample(src, nt, nf, pick) {
  const out = new Uint8Array(CELLS * CELLS);
  for (let y = 0; y < CELLS; y++) {
    const sy0 = Math.min(nt - 1, Math.floor((y * nt) / CELLS));
    const sy1 = Math.max(sy0 + 1, Math.min(nt, Math.ceil(((y + 1) * nt) / CELLS)));
    for (let x = 0; x < CELLS; x++) {
      const sx0 = Math.min(nf - 1, Math.floor((x * nf) / CELLS));
      const sx1 = Math.max(sx0 + 1, Math.min(nf, Math.ceil(((x + 1) * nf) / CELLS)));
      let best = 0;
      for (let sy = sy0; sy < sy1; sy++)
        for (let sx = sx0; sx < sx1; sx++) {
          const v = pick(src, sy * nf + sx);
          if (v > best) best = v;
        }
      out[y * CELLS + x] = best;
    }
  }
  return out;
}

// /api/history's budget is a LEVEL SELECTOR, not a fold target: max_f=256 means "pick the
// level whose natural nf <= 256", which can drop 34x of time resolution AND lose coverage
// the finest level still has (measured: max_f=384 -> level 0, 38784/38784 observed;
// max_f=256 -> level 1, 384/576). So ask for the FINEST affordable grid and fold here.
// T-438's real route should do this fold server-side, the way /api/timeline already does.
const MAX_API_CELLS = 500_000, MAX_AXIS = 4096;

const TIER = { UNOBSERVED: 0, SURVEY: 1, HISTORY: 2, LIVE: 3 };

async function buildTile(lf, lt, fb, tb) {
  const t0wall = performance.now();
  const f0 = fb * fBlock(lf), f1 = f0 + fBlock(lf);
  const t0 = tb * tBlockS(lt), t1 = t0 + tBlockS(lt);

  const qs = `f_lo=${f0}&f_hi=${f1}&t0=${t0}&t1=${t1}`;
  let hist = null, cov = null, err = null;
  try {
    stats.historyCalls++;
    hist = await api(`/api/history?${qs}&max_t=${MAX_AXIS}&max_f=${MAX_AXIS}&max_cells=${MAX_API_CELLS}`);
  } catch (e) { err = String(e.message).slice(0, 160); }
  try {
    stats.coverageCalls++;
    cov = await api(`/api/coverage?f_lo=${f0}&f_hi=${f1}&t0=${t0}&t1=${t1}&cells=${CELLS}&rows=${CELLS}`);
  } catch (e) { err ??= String(e.message).slice(0, 160); }

  const rgb = new Uint8Array(CELLS * CELLS * 3); // all zero => tier 0 => GREY. The default is honest.
  const meta = {
    lf, lt, fb, tb, f0, f1, t0, t1, err,
    requested: { nt: CELLS, nf: CELLS },
    served: hist ? { nt: hist.nt, nf: hist.nf, level: hist.level, t_cell_s: hist.t_cell_s, f_cell_hz: hist.f_cell_hz } : null,
    // THE WELD, MEASURED PER TILE: how far each axis fell short of the square ask.
    shortfall: hist ? { t: CELLS / Math.max(1, hist.nt), f: CELLS / Math.max(1, hist.nf) } : null,
    source: hist?.resolution?.source ?? null,
    live: hist?.resolution?.live ?? false,
    tier: TIER.UNOBSERVED,
    observed_cells: 0,
  };

  if (hist && hist.nt > 0 && hist.nf > 0) {
    // Tier: never claim more than the backend served. If the served cell is coarser than
    // this tile's own cell we REPLICATED, and that is survey-overview, hatched, not detail.
    const coarserT = hist.t_cell_s > tCellS(lt) * 1.0001;
    const coarserF = hist.f_cell_hz > fCell(lf) * 1.0001;
    const tier = hist.resolution?.live ? TIER.LIVE : (coarserT || coarserF) ? TIER.SURVEY : TIER.HISTORY;

    // dB -> 0..255 over the response's own stated range; null stays UNOBSERVED.
    let lo = Infinity, hi = -Infinity;
    for (const v of hist.max_db) if (v != null) { if (v < lo) lo = v; if (v > hi) hi = v; }
    if (!(hi > lo)) { lo = -110; hi = -20; }
    meta.range_db = { lo, hi };
    const val = resample(hist.max_db, hist.nt, hist.nf, (s, i) =>
      s[i] == null ? 0 : Math.max(1, Math.min(255, Math.round(((s[i] - lo) / (hi - lo)) * 254) + 1)));
    const covPlane = resample(hist.coverage, hist.nt, hist.nf, (s, i) =>
      s[i] == null ? 0 : Math.max(0, Math.min(255, Math.round(s[i] * 255))));

    // The record-derived coverage plane (T-423) overrides where it says "never looked":
    // grid.coverage is frames-landed, /api/coverage is what the radio was actually doing.
    let recCov = null;
    const anyDev = cov?.any;
    if (anyDev && Array.isArray(anyDev.cells)) {
      const rn = cov.grid.rows ?? 1, cn = cov.grid.cells;
      recCov = resample(anyDev.cells, rn, cn, (s, i) => {
        const c = s[i];
        if (!c || c.state !== "observed") return 0;
        return Math.max(1, Math.min(255, Math.round((c.duty ?? 1) * 255)));
      });
    }

    let observed = 0;
    for (let i = 0; i < CELLS * CELLS; i++) {
      let c = covPlane[i];
      if (recCov && recCov[i] === 0) c = 0;             // record says unobserved: it wins
      if (recCov && c === 0 && recCov[i] > 0 && val[i] > 0) c = recCov[i];
      const observedHere = c > 0 && val[i] > 0;
      if (observedHere) observed++;
      rgb[i * 3 + 0] = observedHere ? val[i] : 0;
      rgb[i * 3 + 1] = observedHere ? c : 0;
      rgb[i * 3 + 2] = observedHere ? tier : TIER.UNOBSERVED;
    }
    meta.tier = observed ? tier : TIER.UNOBSERVED;
    meta.observed_cells = observed;
  }
  const ms = performance.now() - t0wall;
  stats.tiles++; stats.msTotal += ms;
  stats.byTier[meta.tier] = (stats.byTier[meta.tier] ?? 0) + 1;
  meta.build_ms = +ms.toFixed(2);
  return { rgb, meta };
}

const MIME = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".json": "application/json" };

const server = createServer(async (req, res) => {
  const u = new URL(req.url, "http://x");
  try {
    if (u.pathname === "/spike/tile") {
      const q = (k) => +u.searchParams.get(k);
      const { rgb, meta } = await buildTile(q("lf"), q("lt"), q("fb"), q("tb"));
      res.writeHead(200, {
        "content-type": "application/octet-stream",
        "x-tile-meta": Buffer.from(JSON.stringify(meta)).toString("base64"),
        "cache-control": "no-store",
      });
      return res.end(Buffer.from(rgb));
    }
    if (u.pathname === "/spike/meta") {
      const [state, tl, nav] = await Promise.all([
        api("/api/control/state"), api("/api/timeline?columns=4&rows=2"), api("/api/navigation").catch(() => null),
      ]);
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify({
        window: tl.window,
        device: { id: state.device?.device_id, ranges: state.device?.frequency_ranges_hz, rates: state.device?.sample_rates_hz },
        tuning: state.tuning ?? null,
        live: state.live,
        navigation: nav,
        stats,
      }));
    }
    if (u.pathname === "/spike/retune" && req.method === "POST") {
      // ITEM 3: pan-to-untuned -> retune. THE ONLY PATH TO THE DEVICE IS THE BACKEND'S OWN
      // GATED ROUTES (T-343). There is no second path here, and this one is only reachable
      // from an explicit client gesture that names a destination.
      const body = JSON.parse(await new Promise((r) => { let s = ""; req.on("data", (c) => (s += c)); req.on("end", () => r(s || "{}")); }));
      const post = async (p, b) => {
        const r = await fetch(BACKEND + p, {
          method: "POST", headers: { Authorization: "Bearer " + TOKEN, "content-type": "application/json" },
          body: JSON.stringify(b),
        });
        return { path: p, status: r.status, body: await r.json().catch(() => null) };
      };
      const calls = [];
      if (body.sample_rate_hz) calls.push(await post("/api/control/rate", { sample_rate_hz: body.sample_rate_hz }));
      calls.push(await post("/api/control/center", { center_hz: body.center_hz }));
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(JSON.stringify({ calls, device_id: calls.at(-1)?.body?.device?.id ?? null, action: calls.at(-1)?.body?.device?.action ?? null }));
    }
    // static
    let p = u.pathname === "/" ? "/index.html" : u.pathname;
    if (!/^\/[A-Za-z0-9._\/-]*$/.test(p) || p.includes("..")) { res.writeHead(400); return res.end(); }
    const file = path.join(CLIENT, p);
    if (!existsSync(file)) { res.writeHead(404); return res.end("no " + p); }
    res.writeHead(200, { "content-type": MIME[path.extname(file)] ?? "application/octet-stream" });
    res.end(readFileSync(file));
  } catch (e) {
    res.writeHead(500, { "content-type": "application/json" });
    res.end(JSON.stringify({ error: String(e.message ?? e) }));
  }
});

server.listen(PORT, "127.0.0.1", () => console.log(`t437 spike on http://127.0.0.1:${PORT} -> ${BACKEND}`));
