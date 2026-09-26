// T-1066: `startWatch`/`subscribeChanges` — the client half of `/ws/changes` (T-1065). A watch
// fetches once now, again on a `changed` for its route, and otherwise only on the slow fallback
// clock; a socket drop must not starve it (the fallback keeps it fresh) and a reconnect must pick the
// same subscriptions back up. Drives the real `net.ts` through a WebSocket stub, exactly as
// `presence.test.ts` drives the presence stream — the spy-client rule (T-367): assert what the client
// actually does over the wire, not just that its data ends up right.
import { test } from "node:test";
import assert from "node:assert/strict";
import { backoffMs, startWatch, subscribeChanges } from "../src/app/net";

class FakeSocket {
  static open: FakeSocket[] = [];
  binaryType = "";
  onmessage: ((ev: { data: string | ArrayBuffer }) => void) | null = null;
  onclose: (() => void) | null = null;
  closed = false;
  constructor(readonly url: string) { FakeSocket.open.push(this); }
  close() { this.closed = true; this.onclose?.(); }
  deliver(text: string) { this.onmessage?.({ data: text }); }
}

// `getChangesHub` (net.ts) is a module-level singleton — one `/ws/changes` socket per tab — so every
// case here shares the one hub/socket chain and runs as ONE scenario, in order, the way
// `presence.test.ts` sequences its cases rather than resetting shared module state between them.
(globalThis as Record<string, unknown>).WebSocket = FakeSocket;
(globalThis as Record<string, unknown>).location = { protocol: "http:", host: "h", hash: "", search: "" };
(globalThis as Record<string, unknown>).window = globalThis;
// `takeToken` (net.ts) reads `sessionStorage`, which a headless Node test has none of; a real tab
// always does, so this stands in for it rather than changing what the hub asks for.
const sessionStore = new Map<string, string>();
(globalThis as Record<string, unknown>).sessionStorage = {
  getItem: (k: string) => sessionStore.get(k) ?? null,
  setItem: (k: string, v: string) => { sessionStore.set(k, v); },
  removeItem: (k: string) => { sessionStore.delete(k); },
};
(globalThis as Record<string, unknown>).history = { replaceState: () => {} };

const tick = (ms = 0) => new Promise((r) => setTimeout(r, ms));

test("T-1066: startWatch fetches on `changed`, on the fallback clock only otherwise, and resubscribes after a socket drop", async () => {
  let calls = 0;
  const FALLBACK_MS = 30; // stands in for the real 30_000: the same clock, scaled for a fast test.
  const stop = startWatch("/api/inventory", async () => { calls++; }, FALLBACK_MS);

  await tick();
  assert.equal(calls, 1, "fetches once immediately, like startPoll");

  // No `changed` and short of the fallback: no further request (the acceptance's "30 s, no changed
  // -> <= 1 request", scaled to this test's fallback).
  await tick(15);
  assert.equal(calls, 1, "no request before a `changed` or the fallback clock");

  const sock = FakeSocket.open[0];
  assert.ok(sock, "opened the one shared /ws/changes socket");
  assert.ok(sock.url.includes("/ws/changes"), `expected /ws/changes, got ${sock.url}`);
  sock.deliver(JSON.stringify({ type: "versions", routes: { "/api/inventory": 0 }, tick_ms: 250, t_s: 1 }));

  // A second watch on a DIFFERENT route, to prove `changed` targets only its own subscribers.
  let otherCalls = 0;
  const stopOther = startWatch("/api/annotations", async () => { otherCalls++; }, FALLBACK_MS);
  await tick();
  assert.equal(otherCalls, 1, "the second watch also fetches once immediately");

  sock.deliver(JSON.stringify({ type: "changed", route: "/api/inventory", version: 1, t_s: 2 }));
  await tick();
  assert.equal(calls, 2, "one `changed` triggers exactly one fetch of that route");
  assert.equal(otherCalls, 1, "a `changed` on one route never fetches another");
  stopOther();

  // A socket drop: the fallback keeps the data fresh on its own clock while the hub is reconnecting.
  const callsAtDrop = calls;
  sock.close();
  await tick(FALLBACK_MS + 5);
  assert.ok(calls > callsAtDrop, "the fallback clock kept firing while the socket was down");

  // The hub resubscribes on reconnect: a new socket, and a `changed` on it still reaches this watch.
  await tick(backoffMs(0) + 20);
  const sock2 = FakeSocket.open[1];
  assert.ok(sock2, "reconnected after the drop");
  sock2.deliver(JSON.stringify({ type: "versions", routes: { "/api/inventory": 1 }, tick_ms: 250, t_s: 3 }));
  const before = calls;
  sock2.deliver(JSON.stringify({ type: "changed", route: "/api/inventory", version: 2, t_s: 4 }));
  await tick();
  assert.ok(calls > before, "still subscribed after the socket was replaced");

  stop();
});

test("T-1066: subscribeChanges is the low-level hook startWatch is built on", async () => {
  let n = 0;
  const un = subscribeChanges("/api/outputs", () => { n++; });
  const sock = FakeSocket.open.at(-1)!;
  sock.deliver(JSON.stringify({ type: "changed", route: "/api/outputs", version: 5, t_s: 10 }));
  assert.equal(n, 1, "fired on the matching route");
  sock.deliver(JSON.stringify({ type: "changed", route: "/api/pipelines", version: 1, t_s: 11 }));
  assert.equal(n, 1, "did not fire for a different route");
  un();
  sock.deliver(JSON.stringify({ type: "changed", route: "/api/outputs", version: 6, t_s: 12 }));
  assert.equal(n, 1, "unsubscribed: no further call");
});
