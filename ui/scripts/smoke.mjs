#!/usr/bin/env node
// T-177: headless MUI browser smoke test (not part of npm test / CI). Drives a running
// `hk serve --device mock:...` with playwright-core + a cached "Chrome for Testing" build (as
// used for the T-022a manual smokes; see docs/adr/0010, spikes/s3-web-waterfall/REPORT.md). Not
// added to ui/package.json: playwright-core is installed in a scratch directory instead.
//
// Usage:
//   PW_MODULE_DIR=<dir with node_modules/playwright-core> \
//   SMOKE_BASE=http://127.0.0.1:8931 SMOKE_TOKEN=<token> SMOKE_OUT=<dir for screenshots> \
//   node scripts/smoke.mjs
//
// Exits non-zero if any check fails. Prints a PASS/FAIL line per check per viewport, plus a bug
// list at the end.

import { createRequire } from "node:module";
import { mkdirSync, writeFileSync } from "node:fs";
import path from "node:path";
import { homedir } from "node:os";

const PW_MODULE_DIR = process.env.PW_MODULE_DIR || process.cwd();
const BASE = process.env.SMOKE_BASE || "http://127.0.0.1:8931";
const TOKEN = process.env.SMOKE_TOKEN || "";
const OUT = process.env.SMOKE_OUT || path.join(process.cwd(), "smoke-out");
const CHROMIUM = process.env.CHROMIUM ||
  path.join(homedir(), "Library/Caches/ms-playwright/chromium-1234/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing");

mkdirSync(OUT, { recursive: true });

const require_ = createRequire(path.join(PW_MODULE_DIR, "package.json"));
const { chromium } = require_("playwright-core");

const VIEWPORTS = [
  { name: "desktop", width: 1440, height: 900 },
  { name: "narrow", width: 400, height: 800 },
];

/** @type {{viewport:string, check:string, ok:boolean, detail?:string}[]} */
const results = [];
/** @type {{viewport:string, step:string, kind:string, detail:string}[]} */
const bugs = [];

function record(viewport, check, ok, detail) {
  results.push({ viewport, check, ok, detail });
  console.log(`[${ok ? "PASS" : "FAIL"}] ${viewport} :: ${check}${detail ? " -- " + detail : ""}`);
}

function bug(viewport, step, kind, detail) {
  bugs.push({ viewport, step, kind, detail });
}

async function waitForServer(url, deadlineMs) {
  const start = Date.now();
  let lastErr;
  while (Date.now() - start < deadlineMs) {
    try {
      const res = await fetch(url);
      if (res.status < 500) return true;
    } catch (e) {
      lastErr = e;
    }
    await new Promise((r) => setTimeout(r, 250));
  }
  throw new Error(`server not ready at ${url} within ${deadlineMs}ms: ${lastErr}`);
}

async function shot(page, viewport, step) {
  const file = path.join(OUT, `${viewport}-${step}.png`);
  await page.screenshot({ path: file });
  return file;
}

async function runViewport(browser, vp) {
  const name = vp.name;
  const consoleErrors = [];
  const pageErrors = [];
  const context = await browser.newContext({ viewport: { width: vp.width, height: vp.height } });
  const page = await context.newPage();
  page.on("console", (msg) => {
    if (msg.type() === "error") consoleErrors.push(msg.text());
  });
  page.on("pageerror", (err) => pageErrors.push(String(err && err.stack || err)));

  // Enable the perf counter overlay (localStorage["hk-perf"]) before any script runs.
  await page.addInitScript(() => {
    try { localStorage.setItem("hk-perf", "1"); } catch { /* ignore */ }
  });

  const url = `${BASE}/#token=${encodeURIComponent(TOKEN)}`;
  let loadOk = true;
  try {
    await page.goto(url, { waitUntil: "load", timeout: 20000 });
  } catch (e) {
    loadOk = false;
    bug(name, "load", "navigation", String(e));
  }
  record(name, "page loaded", loadOk);

  // 1. Top bar renders.
  let topBarOk = false;
  try {
    await page.waitForSelector(".bar .brand", { timeout: 10000 });
    const brandText = await page.$eval(".bar .brand", (el) => el.textContent?.trim() || "");
    topBarOk = brandText.toLowerCase().includes("hack");
    if (!topBarOk) bug(name, "top bar", "render", `unexpected brand text: ${JSON.stringify(brandText)}`);
  } catch (e) {
    bug(name, "top bar", "render", String(e));
  }
  record(name, "top bar renders", topBarOk);
  await shot(page, name, "01-top-bar");

  // 2. No horizontal overflow.
  let overflowOk = false;
  let overflowDetail = "";
  try {
    const { scrollWidth, clientWidth } = await page.evaluate(() => ({
      scrollWidth: document.documentElement.scrollWidth,
      clientWidth: document.documentElement.clientWidth,
    }));
    overflowOk = scrollWidth <= clientWidth;
    overflowDetail = `scrollWidth=${scrollWidth} clientWidth=${clientWidth}`;
    if (!overflowOk) bug(name, "overflow", "layout", overflowDetail);
  } catch (e) {
    overflowDetail = String(e);
  }
  record(name, "no horizontal overflow", overflowOk, overflowDetail);

  // 3. Waterfall canvas receives frames within ~10s.
  let waterfallOk = false;
  let waterfallDetail = "";
  try {
    await page.waitForSelector("canvas.live-canvas", { timeout: 10000 });
    const changed = await page.waitForFunction(() => {
      const el = document.querySelector(".c-perf");
      if (!el) return false;
      const text = el.textContent || "";
      const m = text.match(/([\d.]+)\s*fps/);
      if (!m) return false;
      const fps = parseFloat(m[1]);
      window.__hkSmokeFrameSeen = window.__hkSmokeFrameSeen || (fps > 0);
      return window.__hkSmokeFrameSeen && text.length > 0;
    }, { timeout: 10000 }).then(() => true).catch(() => false);

    if (changed) {
      waterfallOk = true;
      waterfallDetail = await page.$eval(".c-perf", (el) => el.textContent || "");
    } else {
      // Fallback: compare canvas pixels before/after ~2s via toDataURL.
      const before = await page.$eval("canvas.live-canvas", (c) => c.toDataURL());
      await page.waitForTimeout(3000);
      const after = await page.$eval("canvas.live-canvas", (c) => c.toDataURL());
      waterfallOk = before !== after;
      waterfallDetail = waterfallOk ? "pixel diff detected" : "canvas pixels unchanged after 3s; perf counter never showed fps";
      if (!waterfallOk) bug(name, "waterfall", "no-frames", waterfallDetail);
    }
  } catch (e) {
    waterfallDetail = String(e);
    bug(name, "waterfall", "error", waterfallDetail);
  }
  record(name, "waterfall receives frames", waterfallOk, waterfallDetail);
  await shot(page, name, "02-waterfall");

  // 4. Switch to Decode mode and back.
  let decodeOk = false;
  try {
    await page.click('.mode[data-mode="decode"]');
    await page.waitForSelector("#view-decode:not([hidden])", { timeout: 5000 });
    await shot(page, name, "03-decode-mode");
    await page.click('.mode[data-mode="explore"]');
    await page.waitForSelector("#view-explore:not([hidden])", { timeout: 5000 });
    decodeOk = true;
  } catch (e) {
    bug(name, "decode mode switch", "interaction", String(e));
  }
  record(name, "decode mode switch (there and back)", decodeOk);
  await shot(page, name, "04-back-to-explore");

  // 5. Open Review drawer and switch through its tabs.
  let reviewOk = false;
  const tabIds = ["alarms", "report", "history", "scheduler", "device", "bookmarks"];
  try {
    await page.click("#review-btn");
    await page.waitForSelector("#review:not([hidden])", { timeout: 5000 });
    await shot(page, name, "05-review-open");
    let allTabsOk = true;
    for (const tabLabel of tabIds) {
      const btn = page.locator(".rv-tabs .rv-tab", { hasText: new RegExp(labelFor(tabLabel), "i") });
      try {
        await btn.click({ timeout: 3000 });
        await page.waitForTimeout(150);
        const selected = await btn.getAttribute("aria-selected");
        if (selected !== "true") {
          allTabsOk = false;
          bug(name, `review tab ${tabLabel}`, "not-selected", `aria-selected=${selected}`);
        }
        await shot(page, name, `06-review-tab-${tabLabel}`);
      } catch (e) {
        allTabsOk = false;
        bug(name, `review tab ${tabLabel}`, "interaction", String(e));
      }
    }
    reviewOk = allTabsOk;
  } catch (e) {
    bug(name, "review drawer", "open", String(e));
  }
  record(name, "review drawer + tabs", reviewOk);

  // 6. Outputs dock and Capture timeline visible.
  let dockOk = false;
  let captureOk = false;
  try {
    const dock = page.locator('[data-slot="outputs"]');
    dockOk = await dock.isVisible();
    if (!dockOk) bug(name, "outputs dock", "not-visible", "footer[data-slot=outputs] not visible");
  } catch (e) {
    bug(name, "outputs dock", "error", String(e));
  }
  record(name, "outputs dock visible", dockOk);
  try {
    const cap = page.locator('[data-slot="capture"]');
    captureOk = await cap.isVisible();
    if (!captureOk) bug(name, "capture timeline", "not-visible", "div[data-slot=capture] not visible");
  } catch (e) {
    bug(name, "capture timeline", "error", String(e));
  }
  record(name, "capture timeline visible", captureOk);
  await shot(page, name, "07-dock-and-timeline");

  // Console / uncaught-exception check (evaluated last so earlier steps' errors are captured).
  const consoleOk = consoleErrors.length === 0;
  if (!consoleOk) bug(name, "console", "console-error", consoleErrors.join(" | "));
  record(name, "no console errors", consoleOk, consoleErrors.slice(0, 3).join(" | "));
  const pageErrOk = pageErrors.length === 0;
  if (!pageErrOk) bug(name, "page", "uncaught-exception", pageErrors.join(" | "));
  record(name, "no uncaught exceptions", pageErrOk, pageErrors.slice(0, 3).join(" | "));

  await context.close();
}

function labelFor(id) {
  return {
    alarms: "Alarms",
    report: "Survey report",
    history: "History",
    scheduler: "Scheduler",
    device: "Device",
    bookmarks: "Bookmarks",
  }[id];
}

async function main() {
  await waitForServer(`${BASE}/api/status?token=${encodeURIComponent(TOKEN)}`, 20000);
  const browser = await chromium.launch({ executablePath: CHROMIUM, headless: true });
  try {
    for (const vp of VIEWPORTS) {
      await runViewport(browser, vp);
    }
  } finally {
    await browser.close();
  }

  writeFileSync(path.join(OUT, "results.json"), JSON.stringify({ results, bugs }, null, 2));
  console.log("\n=== Bugs ===");
  if (bugs.length === 0) console.log("none");
  for (const b of bugs) console.log(`- [${b.viewport}] ${b.step} (${b.kind}): ${b.detail}`);

  const failed = results.filter((r) => !r.ok);
  if (failed.length > 0) {
    console.log(`\n${failed.length} check(s) failed.`);
    process.exit(1);
  }
  console.log("\nAll checks passed.");
}

main().catch((e) => {
  console.error("smoke test crashed:", e);
  process.exit(2);
});
