// T-437 spike: dependency-free Chrome DevTools Protocol driver.
// Node 24 has a built-in WebSocket, so this needs no npm install. Used to get a REAL
// WebGL2 context (ANGLE/Metal on this Mac) rather than a mock, which is the only way the
// upload-count and frame-cost numbers in REPORT.md mean anything.
import { spawn, execFileSync } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import path from "node:path";

const CANDIDATES = [
  path.join(homedir(), "Library/Caches/ms-playwright/chromium-1234/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
  path.join(homedir(), "Library/Caches/ms-playwright/chromium-1226/chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
];

export function findChrome() {
  if (process.env.CHROME && existsSync(process.env.CHROME)) return process.env.CHROME;
  for (const c of CANDIDATES) if (existsSync(c)) return c;
  throw new Error("no Chrome found; set CHROME=/path/to/chrome");
}

export async function launch({ port = 19222, width = 1600, height = 1000, headless = true } = {}) {
  const exe = findChrome();
  const profile = mkdtempSync(path.join(tmpdir(), "t437-chrome-"));
  const args = [
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    `--window-size=${width},${height}`,
    "--no-first-run", "--no-default-browser-check", "--disable-extensions",
    "--use-angle=metal", "--enable-unsafe-webgpu",
    "about:blank",
  ];
  if (headless) args.unshift("--headless=new");
  const proc = spawn(exe, args, { stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  proc.stderr.on("data", (d) => { stderr += d; });

  // Wait for the HTTP endpoint.
  let ws = null;
  for (let i = 0; i < 200; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/json/version`);
      const j = await r.json();
      ws = j.webSocketDebuggerUrl;
      break;
    } catch { await new Promise((r) => setTimeout(r, 100)); }
  }
  if (!ws) { proc.kill(); throw new Error("chrome did not start: " + stderr.slice(0, 2000)); }
  return { proc, port, wsUrl: ws, profile };
}

class Conn {
  constructor(sock) {
    this.sock = sock; this.id = 0; this.pending = new Map(); this.listeners = new Map();
    sock.addEventListener("message", (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id !== undefined) {
        const p = this.pending.get(m.id);
        if (p) { this.pending.delete(m.id); m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result); }
      } else {
        for (const fn of this.listeners.get(m.method) ?? []) fn(m.params);
      }
    });
  }
  send(method, params = {}, sessionId) {
    const id = ++this.id;
    return new Promise((res, rej) => {
      this.pending.set(id, { res, rej });
      this.sock.send(JSON.stringify(sessionId ? { id, method, params, sessionId } : { id, method, params }));
    });
  }
  on(method, fn) {
    if (!this.listeners.has(method)) this.listeners.set(method, []);
    this.listeners.get(method).push(fn);
  }
}

export async function connect(wsUrl) {
  const sock = new WebSocket(wsUrl);
  await new Promise((res, rej) => { sock.addEventListener("open", res); sock.addEventListener("error", rej); });
  return new Conn(sock);
}

// Opens a page target and returns { eval, screenshot, consoleLines, close }.
export async function newPage(conn, url) {
  const { targetId } = await conn.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await conn.send("Target.attachToTarget", { targetId, flatten: true });
  const consoleLines = [];
  conn.on("Runtime.consoleAPICalled", (p) => {
    if (p.sessionId && p.sessionId !== sessionId) return;
    consoleLines.push(p.args.map((a) => (a.value !== undefined ? a.value : a.description ?? a.type)).join(" "));
  });
  conn.on("Runtime.exceptionThrown", (p) => {
    consoleLines.push("EXCEPTION " + (p.exceptionDetails?.exception?.description ?? p.exceptionDetails?.text));
  });
  await conn.send("Runtime.enable", {}, sessionId);
  await conn.send("Page.enable", {}, sessionId);
  if (url) {
    await conn.send("Page.navigate", { url }, sessionId);
    await new Promise((res) => {
      const t = setTimeout(res, 15000);
      conn.on("Page.loadEventFired", () => { clearTimeout(t); res(); });
    });
  }
  const ev = async (expr, awaitPromise = true) => {
    const r = await conn.send("Runtime.evaluate", {
      expression: expr, awaitPromise, returnByValue: true,
    }, sessionId);
    if (r.exceptionDetails) throw new Error("eval threw: " + JSON.stringify(r.exceptionDetails).slice(0, 1500));
    return r.result.value;
  };
  const screenshot = async () => (await conn.send("Page.captureScreenshot", { format: "png" }, sessionId)).data;
  return { sessionId, eval: ev, screenshot, consoleLines };
}

export function kill(browser) {
  try { browser.proc.kill("SIGKILL"); } catch {}
}
