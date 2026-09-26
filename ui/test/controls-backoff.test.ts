// **The shared transport backoff** (T-1035, `ui/src/controls/backoff.ts`): what this client does
// about a server that has stopped answering at all.
//
// The defect this pins: every polling lane paced itself on its own route's floor, so a dead `hk serve`
// was asked by a dozen lanes at once — measured in a browser by `ui/e2e/canvas-journey` test 4 on
// main (2026-09-25): 49 failed requests in the first 5 s after the kill and 29 in the second, with no
// lane individually misbehaving. The ladder here is the one shared answer, and every rule of it is
// asserted below on a FAKE CLOCK: nothing in this file waits for a real timer, and the last test
// drives fifteen pollers over a simulated minute to show the wire cost.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ControlClient, ControlError, reactionTo } from "../src/controls/client";
import { OFFLINE_BASE_MS, OFFLINE_MAX_MS, OfflineError, PROBE_STALE_MS, TransportBackoff } from "../src/controls/backoff";

/** A clock the test moves by hand. */
const clock = (t = 1_000_000) => {
  const c = { t, now: () => c.t, tick: (ms: number) => { c.t += ms; } };
  return c;
};

/** The refused socket a dead server produces: `fetch` REJECTS, with no status at all. */
const refused = () => Object.assign(new TypeError("Failed to fetch"), { name: "TypeError" });

test("the ladder doubles per consecutive silence and is capped", () => {
  const c = clock();
  const g = new TransportBackoff(c.now);
  assert.equal(g.admit(), true, "an unarmed gate admits");
  assert.equal(g.waitMs(), 0);
  g.silent();
  assert.equal(g.waitMs(), OFFLINE_BASE_MS, "the first wait is the base");
  for (const expected of [1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]) {
    c.tick(g.waitMs());
    assert.equal(g.admit(), true, "the gate opens when the wait elapses");
    g.silent();
    assert.equal(g.waitMs(), expected, `wait after ${g.failures} silences`);
  }
  assert.equal(g.waitMs(), OFFLINE_MAX_MS, "and never exceeds the ceiling");
});

test("while armed the gate admits ONE probe, and resets on the first answer", () => {
  const c = clock();
  const g = new TransportBackoff(c.now);
  g.silent();
  assert.equal(g.admit(), false, "inside the wait nothing goes to the wire");
  c.tick(OFFLINE_BASE_MS);
  assert.equal(g.admit(), true, "the first caller after the wait is the probe");
  assert.equal(g.admit(), false, "and it is the only one: fifteen lanes cost one request");
  assert.equal(g.admit(), false);
  g.answered();
  assert.equal(g.failures, 0, "an answer resets the ladder");
  assert.equal(g.waitMs(), 0);
  assert.equal(g.admit(), true, "and the gate is wide open again");
  assert.equal(g.admit(), true);
});

test("a probe that never returns does not lock the gate for ever", () => {
  const c = clock();
  const g = new TransportBackoff(c.now);
  g.silent();
  c.tick(OFFLINE_BASE_MS);
  assert.equal(g.admit(), true);
  assert.equal(g.admit(), false);
  c.tick(PROBE_STALE_MS - 1);
  assert.equal(g.admit(), false, "a probe in flight still holds the slot");
  c.tick(2);
  assert.equal(g.admit(), true, "a lost probe releases it: the slot is not a permanent lock");
});

test("an abort frees the probe slot without moving the ladder", () => {
  const c = clock();
  const g = new TransportBackoff(c.now);
  g.silent();
  const wait = g.waitMs();
  c.tick(wait);
  assert.equal(g.admit(), true);
  g.released();
  assert.equal(g.failures, 1, "the caller's own deadline is evidence about nothing");
  assert.equal(g.admit(), true, "and the next caller may take the freed slot");
});

test("a gated GET never reaches the wire and reads as offline", async () => {
  const c = clock();
  const g = new TransportBackoff(c.now);
  const calls: string[] = [];
  const client = new ControlClient("tok", async (url) => { calls.push(url); throw refused(); }, g);
  await assert.rejects(() => client.get("/api/timeline"), (e) => e instanceof TypeError);
  assert.deepEqual(calls, ["/api/timeline"], "the first ask goes out: that is how silence is learnt");
  assert.equal(g.failures, 1);
  // Every lane's next tick, inside the wait: no wire traffic at all.
  for (let i = 0; i < 20; i++) {
    const e = await client.get("/api/coverage?f0=1").then(() => null, (x: unknown) => x);
    assert.ok(e instanceof OfflineError, "a gated call throws OfflineError");
    assert.match((e as Error).message, /not asking again/);
    assert.equal(reactionTo(e).reaction, "offline",
      "the page says the same thing it says for a refused socket — only the wire differs");
  }
  assert.deepEqual(calls, ["/api/timeline"], "20 polls, one request: the ladder is shared, not per-route");
  // …and the probe, when the wait is out, is exactly one request whoever asks.
  c.tick(OFFLINE_BASE_MS);
  await client.get("/api/coverage?f0=1").catch(() => {});
  await client.get("/api/timeline").catch(() => {});
  await client.get("/api/navigation").catch(() => {});
  assert.deepEqual(calls, ["/api/timeline", "/api/coverage?f0=1"], "one probe, not three");
});

test("an HTTP error is an ANSWER: the ladder resets and the route's error travels on", async () => {
  const g = new TransportBackoff();
  const client = new ControlClient("tok", async () => ({
    ok: false, status: 404, statusText: "Not Found", json: async () => ({ error: "no such band", code: "invalid" }),
  }), g);
  await assert.rejects(() => client.get("/api/coverage"), (e) => e instanceof ControlError && e.status === 404);
  assert.equal(g.failures, 0, "a 404 proves the server is there and talking");
});

test("a user's own press is never gated, and its outcome is evidence like any other", async () => {
  const c = clock();
  const g = new TransportBackoff(c.now);
  const calls: string[] = [];
  let alive = false;
  const client = new ControlClient("tok", async (url) => {
    calls.push(url);
    if (!alive) throw refused();
    return { ok: true, status: 200, statusText: "OK", json: async () => ({ ok: true }) };
  }, g);
  await client.get("/api/timeline").catch(() => {});
  assert.equal(g.failures, 1);
  // A POST inside the wait: a retune must not be swallowed for up to 30 s because a poll failed.
  await assert.rejects(() => client.post("/api/control/center", { hz: 100e6 }), (e) => e instanceof TypeError);
  assert.deepEqual(calls, ["/api/timeline", "/api/control/center"]);
  assert.equal(g.failures, 2, "and its silence raises the ladder like any other");
  // The server comes back; the press that notices resets the gate for every lane.
  alive = true;
  await client.post("/api/control/center", { hz: 100e6 });
  assert.equal(g.failures, 0);
  await client.get("/api/timeline");
  assert.equal(calls.length, 4, "the polls are asking again immediately, with no wait to serve out");
});

test("fifteen pollers over a minute of a dead server cost a bounded, decaying number of requests", async () => {
  // The e2e's subject, in miniature: the app's polling lanes (`startPoll` at 1–10 s, the survey at
  // 2 s, the density layer at 1 s) all ticking against a server that answers nothing, on a fake
  // clock. What is asserted is the shape the browser measured as FLAT: the second half of the
  // minute must cost a small fraction of the first.
  const c = clock();
  const g = new TransportBackoff(c.now);
  let wire = 0;
  const client = new ControlClient("tok", async () => { wire++; throw refused(); }, g);
  const lanes = Array.from({ length: 15 }, (_, i) => 1000 + i * 600);   // 1.0 s … 9.4 s periods
  const perWindow = [0, 0];
  for (let t = 0; t <= 60_000; t += 100) {
    for (const period of lanes) {
      if (t % period !== 0) continue;
      const before = wire;
      await client.get("/api/timeline").catch(() => {});
      perWindow[t < 30_000 ? 0 : 1] += wire - before;
    }
    c.tick(100);
  }
  assert.ok(perWindow[0] <= 12, `first 30 s cost ${perWindow[0]} requests of ${lanes.length} lanes' asks`);
  assert.ok(perWindow[1] <= 2, `second 30 s cost ${perWindow[1]} requests; the ladder is at its ceiling`);
  assert.ok(perWindow[1] <= perWindow[0] * 0.5,
    `the wire cost must DECAY: ${perWindow[0]} then ${perWindow[1]}`);
  // The same loop with no gate is the defect, and it is the control for the numbers above.
  let ungated = 0;
  const plain = new ControlClient("tok", async () => { ungated++; throw refused(); },
    new TransportBackoff(c.now, 0, 0));
  for (let t = 0; t <= 60_000; t += 100) {
    for (const period of lanes) if (t % period === 0) await plain.get("/api/timeline").catch(() => {});
  }
  assert.ok(ungated > 200 && ungated > wire * 10,
    `without the gate the same lanes spend ${ungated} requests against ${wire} with it`);
});
