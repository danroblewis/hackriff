// Spike S3 bench: start the frame server, drive Chromium via playwright-core, collect client stats + CPU.
// Usage (from client/): node ../bench/bench.mjs --bins 4096 --fps 30 [--dtype u8] [--persist gpu] [--secs 60]
//        [--mode headless|headed] [--w 1280 --h 800] [--throttle 1] [--finish 0] [--tag name]
// Env: CHROMIUM=/path/to/chrome (default: Playwright chromium-1234 "Chrome for Testing" in ~/Library/Caches/ms-playwright)
import { spawn, execFileSync } from "node:child_process";
import { mkdirSync, writeFileSync, existsSync } from "node:fs";
import { createRequire } from "node:module";
import { homedir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
// playwright-core is installed as a devDependency of client/ (single lockfile)
const { chromium } = createRequire(path.join(here, "../client/package.json"))("playwright-core");
const argv = process.argv.slice(2);
const arg = (k, d) => { const i = argv.indexOf("--" + k); return i >= 0 ? argv[i + 1] : d; };
const cfg = {
  bins: +arg("bins", 4096), fps: +arg("fps", 30), dtype: arg("dtype", "u8"), persist: arg("persist", "gpu"),
  secs: +arg("secs", 60), mode: arg("mode", "headless"), w: +arg("w", 1280), h: +arg("h", 800),
  throttle: +arg("throttle", 1), finish: arg("finish", "0"), port: +arg("port", 18080), tag: arg("tag", ""),
};
const exe = process.env.CHROMIUM ??
  path.join(homedir(), "Library/Caches/ms-playwright/chromium-1234/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing");
const serverBin = path.join(here, "../server/target/release/s3-frame-server");
const dist = path.join(here, "../client/dist");
if (!existsSync(serverBin)) throw new Error("build the server first: cargo build --release (in server/)");

// ---- CPU time accounting via ps (cumulative cputime of process trees) ----
function psTable() {
  const out = execFileSync("ps", ["-A", "-o", "pid=,ppid=,time="], { encoding: "utf8" });
  return out.trim().split("\n").map((l) => {
    const [pid, ppid, t] = l.trim().split(/\s+/);
    const parts = t.split(":").map(Number); // [[h:]m:ss.ss]
    const secs = parts.reduce((a, v) => a * 60 + v, 0);
    return { pid: +pid, ppid: +ppid, secs };
  });
}
function treeSecs(rootPids, excludePids = []) {
  const rows = psTable();
  const kids = new Map();
  for (const r of rows) { if (!kids.has(r.ppid)) kids.set(r.ppid, []); kids.get(r.ppid).push(r); }
  const byPid = new Map(rows.map((r) => [r.pid, r]));
  const seen = new Map();
  const stack = rootPids.map((p) => byPid.get(p)).filter(Boolean);
  while (stack.length) {
    const r = stack.pop();
    if (seen.has(r.pid) || excludePids.includes(r.pid)) continue;
    seen.set(r.pid, r.secs);
    for (const k of kids.get(r.pid) ?? []) stack.push(k);
  }
  return seen; // pid -> cputime secs
}
function cpuDelta(a, b, wall) {
  let s = 0;
  for (const [pid, t] of b) s += t - (a.get(pid) ?? 0); // new processes count from 0
  return (100 * s) / wall; // % of one core
}

const server = spawn(serverBin, ["--bind", `127.0.0.1:${cfg.port}`, "--dist", dist], { stdio: ["ignore", "ignore", "pipe"] });
let serverLog = "";
server.stderr.on("data", (d) => (serverLog += d));
await new Promise((r) => setTimeout(r, 500));

const browser = await chromium.launch({
  executablePath: exe,
  headless: cfg.mode !== "headed",
  args: [
    "--use-angle=metal", "--enable-gpu", "--ignore-gpu-blocklist",
    "--disable-background-timer-throttling", "--disable-renderer-backgrounding", "--disable-backgrounding-occluded-windows",
  ],
});
const ctx = await browser.newContext({ viewport: { width: cfg.w, height: cfg.h }, deviceScaleFactor: 1 });
const page = await ctx.newPage();
page.on("pageerror", (e) => console.error("pageerror:", e.message));
page.on("console", (m) => { if (m.type() === "error") console.error("console:", m.text()); });
if (cfg.throttle > 1) {
  const cdp = await ctx.newCDPSession(page);
  await cdp.send("Emulation.setCPUThrottlingRate", { rate: cfg.throttle });
}
const url = `http://127.0.0.1:${cfg.port}/?bins=${cfg.bins}&fps=${cfg.fps}&dtype=${cfg.dtype}&persist=${cfg.persist}&finish=${cfg.finish}`;
await page.goto(url);
await page.waitForFunction(() => window.__s3 && window.__s3.rx > 10, null, { timeout: 15000 });
await page.waitForTimeout(3000); // warm-up
await page.evaluate(() => window.__s3reset());

const browserRoots = [process.pid];
const excl = [server.pid];
const b0 = treeSecs(browserRoots, excl), s0 = treeSecs([server.pid]);
const t0 = Date.now();
await page.waitForTimeout(cfg.secs * 1000);
const wall = (Date.now() - t0) / 1000;
const b1 = treeSecs(browserRoots, excl), s1 = treeSecs([server.pid]);
// note: node (this bench script) is in the browser tree root; it is near-idle during the wait and is reported inside browser CPU.

const stats = await page.evaluate(() => {
  const S = window.__s3;
  const pct = (a, p) => { if (!a.length) return null; const s = [...a].sort((x, y) => x - y); return +s[Math.min(s.length - 1, Math.floor(p * s.length))].toFixed(2); };
  const iv = []; for (let i = 1; i < S.rafTimes.length; i++) iv.push(S.rafTimes[i] - S.rafTimes[i - 1]);
  const span = (S.rafTimes[S.rafTimes.length - 1] - S.rafTimes[0]) / 1000;
  return {
    renderer: S.renderer, maxTex: S.maxTex, texW: S.texW, floatRT: S.floatRT,
    canvas: [document.getElementById("c").width, document.getElementById("c").height],
    rafFps: +((S.rafTimes.length - 1) / span).toFixed(2),
    frameMs: { p50: pct(iv, 0.5), p99: pct(iv, 0.99), max: pct(iv, 1) },
    longFrames33: iv.filter((x) => x > 33.4).length,
    dataRowsPerSec: +(S.rendered / span).toFixed(2),
    rx: S.rx, rendered: S.rendered, seqGaps: S.gaps, clientDropped: S.clientDropped,
    rxMBps: +(S.rxBytes / 1e6 / span).toFixed(3),
    latTsMs: { p50: pct(S.latTsMs, 0.5), p99: pct(S.latTsMs, 0.99) },
    latRxMs: { p50: pct(S.latRxMs, 0.5), p99: pct(S.latRxMs, 0.99) },
    workMs: { p50: pct(S.workMs, 0.5), p99: pct(S.workMs, 0.99) },
    ingestMs: { p50: pct(S.ingestMs, 0.5), p99: pct(S.ingestMs, 0.99) },
  };
});
if (arg("shot")) await page.screenshot({ path: arg("shot") });
await browser.close();
server.kill("SIGINT");

const result = {
  date: new Date().toISOString(), cfg, url, wallSecs: +wall.toFixed(1),
  browserCpuPct: +cpuDelta(b0, b1, wall).toFixed(1), serverCpuPct: +cpuDelta(s0, s1, wall).toFixed(1),
  ...stats,
};
mkdirSync(path.join(here, "../results"), { recursive: true });
const name = `${cfg.tag ? cfg.tag + "-" : ""}${cfg.mode}-b${cfg.bins}-f${cfg.fps}-${cfg.dtype}-${cfg.persist}${cfg.throttle > 1 ? "-x" + cfg.throttle : ""}${cfg.finish === "1" ? "-finish" : cfg.finish === "2" ? "-readpixels" : ""}-${cfg.w}x${cfg.h}.json`;
writeFileSync(path.join(here, "../results", name), JSON.stringify(result, null, 2));
console.log(JSON.stringify(result));
