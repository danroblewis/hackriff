// `npm run e2e:selftest-crosstalk`: proves T-883 — two e2e runs on one machine (two worktrees, or an
// agent beside the gate) can never adopt, report on or kill each other's processes.
//
// Needs no browser, no `ui/dist` and no `hk` build: it drives the real `startBackend` and the real
// `run.mjs` startup sweep, with a FAKE `hk` (HK_BIN) standing in for the server so the race can be
// forced deterministically instead of waited for.
//
// **Leg 1 — the port race.** `freePort` answers "free" at the instant it asks and `hk serve` binds a
// moment later, so two runs asking about one port at once are both told it is free and one bind
// loses. Measured with the real binary before the fix: two concurrent `startBackend` calls on one
// port returned the SAME origin 3/3 times, and one of them owned a child that had already died of
// the bind error — it had adopted the other run's server (and, with the old shared token, every
// authenticated read against it succeeded). Here the fake `hk` plays the loser exactly: the first time
// it runs, another "run's" server takes the port first and the fake then dies of "address in use".
// Red on the old code (it returns the foreign origin); green once readiness demands identity.
//
// **Leg 2 — the sweep is this checkout's own.** A lock left by a dead run in ANOTHER checkout, naming
// a live child that passes every identity check, must be left alone; the same lock in THIS checkout
// must still be swept (T-740's SIGKILL cleanup, unchanged for the run it belongs to).
import { spawn, spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, unlinkSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const TMP = mkdtempSync(path.join(os.tmpdir(), "hk-e2e-selftest-crosstalk-"));
let ok = true;
const check = (cond, msg) => { console.log(`e2e selftest-crosstalk: ${cond ? "ok  " : "FAIL"} ${msg}`); if (!cond) ok = false; };

// ---------------------------------------------------------------------------------------------------
// Leg 1: the port race
// ---------------------------------------------------------------------------------------------------
const FOREIGN_PID_FILE = path.join(TMP, "foreign.pid");
// A tiny stand-in for another worktree's e2e `hk serve`: answers the readiness probe, and accepts the
// token every e2e backend used to share — exactly what the losing run used to meet.
const FOREIGN = path.join(TMP, "foreign.mjs");
writeFileSync(FOREIGN, `
import http from "node:http";
const port = Number(process.argv[2]);
http.createServer((req, res) => {
  const u = new URL(req.url, "http://x");
  res.setHeader("x-owner", "foreign-" + process.pid);
  if (u.pathname.startsWith("/api/")) {
    res.statusCode = u.searchParams.get("token") === "hke2e0123456789abcdef" ? 200 : 401;
    return res.end("{}");
  }
  res.end("<html></html>");
}).listen(port, "127.0.0.1", () => console.log("listening"));
setTimeout(() => process.exit(0), 120000);
`);
// The fake `hk`: loses the race the first time it is run, and behaves as a real server after.
const FAKE_HK = path.join(TMP, "hk");
writeFileSync(FAKE_HK, `#!${process.execPath}
import http from "node:http";
import { spawn } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
const args = process.argv.slice(2);
const port = Number(args[args.indexOf("--bind") + 1].split(":")[1]);
const once = ${JSON.stringify(FOREIGN_PID_FILE)};
if (!existsSync(once)) {
  // Someone else binds the port between the harness's "is it free?" and our bind.
  const f = spawn(process.execPath, [${JSON.stringify(FOREIGN)}, String(port)], { detached: true, stdio: ["ignore", "pipe", "ignore"] });
  writeFileSync(once, String(f.pid));
  await new Promise((r) => f.stdout.once("data", r));
  f.unref();
  // A real hk serve spends a moment on the fixture and the data dir before it binds and dies.
  await new Promise((r) => setTimeout(r, 1500));
  console.error("Error: failed to bind 127.0.0.1:" + port + ": Address already in use (os error 48)");
  process.exit(1);
}
http.createServer((req, res) => {
  const u = new URL(req.url, "http://x");
  res.setHeader("x-owner", String(process.pid));
  if (u.pathname.startsWith("/api/")) {
    res.statusCode = u.searchParams.get("token") === process.env.HK_TOKEN ? 200 : 401;
    return res.end("{}");
  }
  res.end("<html></html>");
}).listen(port, "127.0.0.1");
`);
chmodSync(FAKE_HK, 0o755);
const DIST = path.join(TMP, "dist");
mkdirSync(DIST);
writeFileSync(path.join(DIST, "surface.html"), "<html></html>");

process.env.HK_BIN = FAKE_HK;
const { startBackend } = await import("./backend.mjs");
// Above every gate lane (8791..) and every work-runner block (9216 + 256k, k < 146, so < 46592) and
// below macOS's ephemeral range (49152), so it never takes a port a real run wants.
const backend = await startBackend({ port: Number(process.env.HK_E2E_SELFTEST_PORT ?? 46800), uiDist: DIST });
try {
  const r = await fetch(`${backend.origin}/surface.html`);
  const owner = r.headers.get("x-owner");
  check(owner === String(backend.proc.pid),
    `startBackend returned a server this call started (answered by "${owner}", own child pid ${backend.proc.pid}, ${backend.origin})`);
  check(backend.proc.exitCode === null, "startBackend's own child is alive");
  const bad = await fetch(`${backend.origin}/api/streams?token=hke2e0123456789abcdef`);
  check(bad.status === 401, `the backend's token is its own, not the constant every run shared (constant got ${bad.status})`);
} finally {
  backend.stop();
  try { process.kill(Number(readFileSync(FOREIGN_PID_FILE, "utf8")), "SIGKILL"); } catch { /* gone */ }
}

// ---------------------------------------------------------------------------------------------------
// Leg 2: the sweep only judges this checkout's own runs
// ---------------------------------------------------------------------------------------------------
const LOCK_DIR = path.join(os.tmpdir(), "hk-e2e-runs");
mkdirSync(LOCK_DIR, { recursive: true });
const lstart = (pid) => spawnSync("ps", ["-o", "lstart=", "-p", String(pid)], { encoding: "utf8" }).stdout.trim();

async function liveChild(marker) {
  const c = spawn(process.execPath, ["-e", "setTimeout(() => {}, 60000)", marker], { stdio: "ignore" });
  c.exited = new Promise((r) => c.once("exit", () => r(true)));
  await new Promise((r) => setTimeout(r, 300));
  return c;
}
// Our own children, so "gone" is their `exit` event: a killed child this process has not reaped yet
// is a zombie, and `kill(pid, 0)` still answers for a zombie.
const diedWithin = (c, ms) => Promise.race([c.exited, new Promise((r) => setTimeout(() => r(false), ms))]);
function fakeLock(root, child, marker) {
  const deadOwner = spawnSync("true", []).pid; // exits at once: a run that was SIGKILLed
  const file = path.join(LOCK_DIR, `${deadOwner}.json`);
  writeFileSync(file, JSON.stringify({
    pid: deadOwner, root, startedAt: Date.now(),
    children: [{ pid: child.pid, marker, lstart: lstart(child.pid) }],
  }));
  return file;
}

const otherMarker = `hk-e2e-data-selftest-crosstalk-other-${process.pid}`;
const ownMarker = `hk-e2e-data-selftest-crosstalk-own-${process.pid}`;
const other = await liveChild(otherMarker);
const own = await liveChild(ownMarker);
const otherLock = fakeLock(path.join(TMP, "another-worktree", "ui", "e2e"), other, otherMarker);
const ownLock = fakeLock(HERE, own, ownMarker);

const run = spawnSync(process.execPath, [path.join(HERE, "run.mjs"), "__never_matches_any_real_spec__"], {
  cwd: HERE, encoding: "utf8",
});
const out = `${run.stdout ?? ""}${run.stderr ?? ""}`;
console.log(out.trim().split("\n").map((l) => `  | ${l}`).join("\n"));
check(!(await diedWithin(other, 1000)), `another checkout's verified child (pid ${other.pid}) survives this checkout's sweep`);
check(await diedWithin(own, 5000), `this checkout's own dead run's child (pid ${own.pid}) is still swept`);
for (const c of [other, own]) { try { process.kill(c.pid, "SIGKILL"); } catch { /* gone */ } }
for (const f of [otherLock, ownLock]) { try { unlinkSync(f); } catch { /* swept */ } }

rmSync(TMP, { recursive: true, force: true });
console.log(ok
  ? "\nPASS: a lost port race is never adopted (a run only ever drives its own hk serve), and the startup sweep only judges this checkout's own runs."
  : "\nFAIL: runs in different checkouts can still adopt or kill each other's processes. See the flags above.");
process.exit(ok ? 0 : 1);
