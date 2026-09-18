// A dependency-free Chrome DevTools Protocol driver (T-455, promoted from the T-437 spike's
// `harness/cdp.mjs`, which was written for the one-off WebGL2 proof and never committed).
//
// **Why this rather than Playwright.** The T-455 brief allowed either. What this tier has to do is
// narrow — navigate, watch the network, run an expression, take a screenshot — and all four are
// single CDP calls. Playwright's value is the part we do not need (selector engines, auto-waiting,
// cross-browser, trace viewer) and its cost is the part that decides it: a ~170 MB browser download
// per machine and a CI cache to manage. Node 24 ships a `WebSocket`, so this file needs no `npm
// install` at all, and it drives **whatever Chrome the machine already has** — the ms-playwright
// cache on the dev Mac, `google-chrome` on a GitHub runner image. The dependency cost of this whole
// tier is therefore zero new packages and zero new downloads. See `ui/e2e/README.md`.
//
// The trade we are accepting: no auto-waiting. Every wait in this suite is therefore an explicit,
// named condition in `harness.mjs` (`waitFor`), which is the honest form anyway — a flaky sleep
// would be the thing that gets this tier disabled.
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import path from "node:path";

/** Every Chrome this suite knows how to find, most-specific first. */
function candidates() {
  const out = [];
  if (process.env.CHROME) out.push(process.env.CHROME);
  // The playwright browser cache, if some other tool on this machine populated it. Version
  // directories change, so they are globbed rather than pinned (the spike hard-coded 1226/1234 and
  // would have gone stale on the next update).
  const cache = path.join(homedir(), "Library/Caches/ms-playwright");
  const linuxCache = path.join(homedir(), ".cache/ms-playwright");
  for (const root of [cache, linuxCache]) {
    let names = [];
    try { names = readdirSync(root); } catch { /* no cache on this machine */ }
    for (const n of names.sort().reverse()) {
      if (!n.startsWith("chromium")) continue;
      out.push(
        path.join(root, n, "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
        path.join(root, n, "chrome-mac/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing"),
        path.join(root, n, "chrome-linux/chrome"),
        path.join(root, n, "chrome-linux/headless_shell"),
      );
    }
  }
  out.push(
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    // GitHub's ubuntu runner images ship all three of these preinstalled, which is why this tier
    // needs no browser-install step in CI.
    "/usr/bin/google-chrome", "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium", "/usr/bin/chromium-browser",
  );
  return out;
}

export function findChrome() {
  for (const c of candidates()) if (c && existsSync(c)) return c;
  throw new Error(
    "no Chrome found for the browser tier. Set CHROME=/path/to/chrome, or install one:\n" +
    "  macOS: Google Chrome, or `npx --yes playwright@1 install chromium` (populates ~/Library/Caches/ms-playwright)\n" +
    "  linux: apt-get install -y chromium  (GitHub's ubuntu runner images already ship google-chrome)\n" +
    "Looked at:\n  " + candidates().join("\n  "),
  );
}

export async function launch({ port = 19455, width = 1440, height = 900, headless = true } = {}) {
  const exe = findChrome();
  const profile = mkdtempSync(path.join(tmpdir(), "hk-e2e-chrome-"));
  const args = [
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    `--window-size=${width},${height}`,
    "--no-first-run", "--no-default-browser-check", "--disable-extensions",
    "--disable-background-timer-throttling", "--disable-renderer-backgrounding",
    // A real WebGL2 context: the platform's own ANGLE backend (Metal here, GL on a CI runner),
    // with the software rasterizer allowed as a fallback so a headless box with no GPU still
    // renders rather than silently drawing nothing. Both are real contexts running the real
    // shader; what this tier must not do is accept a page that draws nothing at all.
    "--use-angle=default", "--enable-unsafe-swiftshader",
    "about:blank",
  ];
  if (headless) args.unshift("--headless=new");
  // `detached` puts Chrome in its own process group so `kill()` below can take the whole tree.
  // SIGKILL on the parent alone leaves its ~20 renderer and GPU children reparented and running,
  // and a test tier that leaks twenty processes per run is a test tier people disable.
  const proc = spawn(exe, args, { stdio: ["ignore", "ignore", "pipe"], detached: true });
  let stderr = "";
  proc.stderr.on("data", (d) => { stderr += d; });

  let ws = null;
  for (let i = 0; i < 300; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${port}/json/version`);
      ws = (await r.json()).webSocketDebuggerUrl;
      break;
    } catch { await new Promise((r) => setTimeout(r, 50)); }
  }
  if (!ws) { killTree(proc); throw new Error(`chrome did not start: ${stderr.slice(0, 2000)}`); }
  return { proc, exe, port, wsUrl: ws, profile };
}

function killTree(proc) {
  // The group first (negative pid), then the parent as a fallback for a platform where the group
  // is gone but the process is not.
  try { process.kill(-proc.pid, "SIGKILL"); } catch { /* group already gone */ }
  try { proc.kill("SIGKILL"); } catch { /* already gone */ }
}

class Conn {
  constructor(sock) {
    this.sock = sock; this.id = 0; this.pending = new Map(); this.listeners = new Map();
    sock.addEventListener("message", (ev) => {
      const m = JSON.parse(ev.data);
      if (m.id !== undefined) {
        const p = this.pending.get(m.id);
        if (!p) return;
        this.pending.delete(m.id);
        m.error ? p.rej(new Error(JSON.stringify(m.error))) : p.res(m.result);
      } else {
        for (const fn of this.listeners.get(m.method) ?? []) fn(m.params, m.sessionId);
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
  close() { try { this.sock.close(); } catch { /* already gone */ } }
}

export async function connect(wsUrl) {
  const sock = new WebSocket(wsUrl);
  await new Promise((res, rej) => {
    sock.addEventListener("open", res);
    sock.addEventListener("error", () => rej(new Error("devtools websocket refused")));
  });
  return new Conn(sock);
}

export function kill(browser) {
  killTree(browser.proc);
  try { rmSync(browser.profile, { recursive: true, force: true }); } catch { /* best effort */ }
}
