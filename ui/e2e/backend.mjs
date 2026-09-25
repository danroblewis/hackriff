// The real backend, under the browser tier (T-455).
//
// **Why `hk serve` and not a node mock.** T-450's defect was that the built bundle threw during
// module evaluation because `hk serve` sends `default-src 'self'` with no `unsafe-eval`. That
// header is a constant in `crates/hk-api/src/http.rs`. A node mock would have to restate it, which
// makes a second copy of the load-bearing fact — the exact drift shape `cellrule.ts` was rewritten
// to avoid. So this tier runs the product's own server, over a **recorded** fixture, and the header
// under test is the one the product ships. `assertRealCsp` below then checks the server actually
// sent the no-`unsafe-eval` policy, so a future loosening of the CSP cannot quietly make T-450's
// guard vacuous.
//
// The fixture is a SigMF recording replayed through `--replay`; nothing here touches a radio and
// nothing here can retune one.
import { spawn } from "node:child_process";
import net from "node:net";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const UI_DIR = path.resolve(fileURLToPath(import.meta.url), "../..");
export const REPO = path.resolve(UI_DIR, "..");

/** Ports this suite may never take: the user's live-HackRF demo and the stream/API ports beside it. */
const FORBIDDEN = new Set([8788, 8789, 8899, 8900]);

export const DEFAULT_FIXTURE = "fixtures/hackrf/2026-09-13/fm_100p8M_2p4M_l32g30a1_t1p5_5s.sigmf-meta";

/** Can this process actually take the port? The real question, asked the only way that answers it. */
const canBind = (port) => new Promise((res) => {
  const s = net.createServer();
  s.once("error", () => res(false));
  s.listen(port, "127.0.0.1", () => s.close(() => res(true)));
});

/**
 * The first port from `first` that this run can actually take, skipping the reserved ones.
 *
 * Two questions, and both have to be asked. **Is something SERVING it** — an HTTP response to
 * `/surface.html`, the same question the readiness loop asks, because that is exactly what the
 * readiness loop would mistake for its own server (T-470). And **can this process BIND it**,
 * because "nothing speaks HTTP here" is not the same as "this port is free": `canvas-journey`'s
 * test 4 holds the port of the server it killed with a socket that refuses every connection, so
 * that nobody can take it over mid-measurement — which answers the HTTP probe exactly like an empty
 * port and would hand this caller a port whose bind then fails and kills `hk serve` on startup.
 * A bind test costs one syscall and cannot be fooled by what the occupant chooses to say.
 */
async function freePort(first, tries = 24) {
  for (let p = first; p < first + tries; p++) {
    if (FORBIDDEN.has(p)) continue;
    let serving = false;
    try {
      await fetch(`http://127.0.0.1:${p}/surface.html`, { signal: AbortSignal.timeout(1500) });
      serving = true;
    } catch { /* nothing listening, or nothing that speaks HTTP — still has to be bindable */ }
    if (!serving && await canBind(p)) return p;
    if (p === first) {
      console.error(`e2e: port ${p} is already ${serving ? "serving" : "bound"} — another e2e run is using ` +
        "it; stepping past it rather than testing that run's bundle");
    }
  }
  throw new Error(`no free port in ${first}..${first + tries - 1}: every one is already taken. ` +
    "Another e2e run (or several) is in flight; wait for it, or set HK_E2E_PORT.");
}

export function hkBinary() {
  if (process.env.HK_BIN) {
    if (!existsSync(process.env.HK_BIN)) throw new Error(`HK_BIN=${process.env.HK_BIN} does not exist`);
    return process.env.HK_BIN;
  }
  // CARGO_TARGET_DIR first: the merge runner builds in its own target dir so main's target/ stays a
  // stable clone source for worker worktrees (2026-09-24: every gate rebuild of target/ in place made
  // the shared blocks of every cloned worktree target exclusive, ~2 GB/min of disk).
  const dirs = [process.env.CARGO_TARGET_DIR, "target"].filter(Boolean);
  for (const d of dirs) {
    for (const p of ["release/hk", "debug/hk"]) {
      const abs = path.resolve(REPO, d, p);
      if (existsSync(abs)) return abs;
    }
  }
  throw new Error(
    "no `hk` binary. Build it once (`cargo build -p hk-cli --bin hk`) or set HK_BIN=/path/to/hk.\n" +
    "This tier drives the product's own server so the CSP and the /api/tiles backpressure under\n" +
    "test are the real ones; see the header of ui/e2e/backend.mjs.",
  );
}

/**
 * Start `hk serve` over a recorded fixture and wait until it is answering.
 *
 * Returns `{ origin, token, stop(), log() }`. `stop()` is idempotent and always kills the child.
 */
export async function startBackend({
  port = Number(process.env.HK_E2E_PORT ?? 8791),
  fixture = process.env.HK_E2E_FIXTURE ?? DEFAULT_FIXTURE,
  // T-476, additive and off by default: drive the **mock SDR device** over the same fixture
  // (`--device mock:…`) instead of a plain `--replay`. A replay reports no frequency grid and is not
  // live, so every device-facing control on the page is correctly stated-and-disabled — which proves
  // the disabled half and nothing about the enabled one. The mock reports a HackRF-class grid, an
  // active capture window, and takes a retune, so a browser can drive the whole act without any real
  // hardware (CLAUDE.md: e2e goes THROUGH the device interface; receive only; never the real radio).
  mockDevice = false,
  // T-508: a **fault** for that mock device (`HK_MOCK_FAULT`, e.g. `retune-apply-fails:1` or
  // `gone-on-retune`; see `hk_core::MockFault::parse`). Every retune guard in this tier was green
  // because the mock always landed exactly where it was told; a fault is what lets one go red. It
  // reaches only `--device mock:…` — a real radio has no such switch — so it needs `mockDevice`.
  mockFault = null,
  // HK_E2E_UI_DIST is how `selftest.mjs` points the product's own server at a DELIBERATELY BROKEN
  // build, to prove this suite can still tell the difference.
  uiDist = process.env.HK_E2E_UI_DIST ?? path.join(UI_DIR, "dist"),
  token = "hke2e0123456789abcdef",
  // T-845, additive: further `hk serve` flags, e.g. `["--iq-retention", "20s"]` for a ring that
  // wraps within seconds rather than after the default two minutes.
  args = [],
} = {}) {
  if (FORBIDDEN.has(port)) {
    throw new Error(`port ${port} is reserved (the user's live-HackRF demo and its stream port); pick another`);
  }
  if (!existsSync(path.join(uiDist, "surface.html"))) {
    throw new Error(`${uiDist}/surface.html is missing — run \`npm run build\` in ui/ first`);
  }
  // **Never adopt somebody else's server** (T-470). The readiness loop below waits for *anything* to
  // answer `/surface.html` on this port, and a second worktree running its own e2e on the default
  // port answers it — so the suite drives **that worktree's bundle**, reports on code the run never
  // built, and goes green or red about the wrong thing. Measured the expensive way: three runs of a
  // new guard failed against a page with none of the code under test, because another agent's
  // `hk serve` held 8791. The repo runs up to four agents at once, so this is the normal case, not a
  // corner. Stepping to the next free port is what the caller wanted anyway — the port is internal,
  // callers use the returned `origin` — and it fails closed if none is free.
  if (mockFault && !mockDevice) throw new Error("a mock fault needs mockDevice: true (a --replay has no device to fail)");
  port = await freePort(port);
  const bin = hkBinary();
  const dataDir = mkdtempSync(path.join(tmpdir(), "hk-e2e-data-"));
  const source = mockDevice
    ? ["--device", `mock:${path.join(REPO, fixture)}`]
    : ["--replay", path.join(REPO, fixture), "--loop"];
  const proc = spawn(bin, [
    "serve",
    ...source,
    "--bind", `127.0.0.1:${port}`,
    "--data-dir", dataDir,
    "--ui-dist", uiDist,
    ...args,
  ], {
    cwd: REPO,
    // HK_STREAM_TCP is pinned away from the default as well: `hk serve` also runs a TCP stream
    // server, whose default is 8788 — the port beside the user's live-HackRF demo. `:0` asks the
    // OS for an ephemeral one, so this tier can never take a port anything else wants.
    env: {
      ...process.env, HK_TOKEN: token, HK_STREAM_TCP: "127.0.0.1:0",
      // Always set, so a fault in the caller's own environment can never leak into a run that
      // did not ask for one ("" parses as no fault).
      HK_MOCK_FAULT: mockFault ?? "",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  let log = "";
  proc.stdout.on("data", (d) => { log += d; });
  proc.stderr.on("data", (d) => { log += d; });
  let exited = null;
  proc.on("exit", (code, sig) => { exited = `hk serve exited early (code ${code}, signal ${sig})`; });

  const origin = `http://127.0.0.1:${port}`;
  const stop = () => {
    try { proc.kill("SIGKILL"); } catch { /* already gone */ }
    try { rmSync(dataDir, { recursive: true, force: true }); } catch { /* best effort */ }
  };

  for (let i = 0; i < 400; i++) {
    if (exited) { stop(); throw new Error(`${exited}\n${log.slice(-2000)}`); }
    try {
      const r = await fetch(`${origin}/surface.html`);
      if (r.ok) return { origin, token, dataDir, stop, log: () => log, proc };
    } catch { /* not listening yet */ }
    await new Promise((r) => setTimeout(r, 50));
  }
  stop();
  throw new Error(`hk serve did not come up on ${origin} in 20 s:\n${log.slice(-2000)}`);
}

/**
 * The header T-450 died on, asserted at the source rather than assumed.
 *
 * If someone adds `unsafe-eval` to the product's CSP, `surface-load.e2e.mjs` would keep passing
 * while proving nothing — so the guard checks its own premise first.
 */
export async function assertRealCsp(origin) {
  const r = await fetch(`${origin}/surface.html`);
  const csp = r.headers.get("content-security-policy") ?? "";
  if (!/default-src\s+'self'/.test(csp)) {
    throw new Error(`the server did not send a default-src 'self' CSP, so the T-450 guard proves nothing: ${csp || "(no header)"}`);
  }
  if (/unsafe-eval/.test(csp)) {
    throw new Error(`the product CSP now allows unsafe-eval, so the T-450 guard is vacuous: ${csp}`);
  }
  return csp;
}

/**
 * `/api/tiles`' own declared backpressure cap — the number the client must obey, from the server.
 *
 * Retries a `503`: a just-started `hk serve` is ingesting the recording and holding the history
 * lock, so the route legitimately refuses for the first seconds. Obeying the refusal is exactly
 * what the route asks a client to do, and a harness that could not do it would be a poor witness
 * against a client that cannot either.
 */
export async function tileCost(origin, token, { timeoutMs = 60000 } = {}) {
  const q = new URLSearchParams({
    token, level_f: "0", level_t: "0", f_index: "0", t_index: "0", cells: "8",
  });
  const t0 = Date.now();
  for (;;) {
    const r = await fetch(`${origin}/api/tiles?${q}`);
    const j = await r.json().catch(() => ({}));
    if (r.ok) return j.cost ?? {};
    if (r.status !== 503 || Date.now() - t0 > timeoutMs) {
      throw new Error(`GET /api/tiles failed (${r.status}) after ${Date.now() - t0} ms: ${JSON.stringify(j).slice(0, 400)}`);
    }
    await new Promise((res) => setTimeout(res, 250));
  }
}
